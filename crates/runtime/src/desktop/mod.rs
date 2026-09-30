//! Desktop capability profile of the shared Harness runtime.
//!
//! This module retains the existing desktop assembly, session ownership and
//! lifecycle behavior. CLI and RPC hosts consume the same implementation.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 13 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 5 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

pub mod agent_restore;
mod agent_skill_loader;
pub mod auto_mode_propose;
mod sdk;
pub use sdk::build_harness;

/// Session-owned teammate registry shared with CLI hosts.
pub use coordinator::TeamRegistry;

/// Shared teammate envelope used by host message queues.
pub use tasks::handlers::in_process_teammate::teammate_message_envelope_with_summary;

mod background_agent;
mod connect;
mod cron_command;
pub mod cron_management;
mod cron_native;
pub mod file_changed_watch;
pub mod fork_resume;
#[cfg(test)]
mod fusion_attempt_composition_test;
mod fusion_attempts;
mod fusion_command;
mod fusion_implement;
#[cfg(test)]
mod fusion_implement_e2e_test;
#[cfg(test)]
mod fusion_pool_admission_test;
pub mod fusion_recorder;
pub mod ide;
mod pane_teammate;
mod sandbox_runner;
pub mod session_agents;
pub mod session_state;
pub mod settings_watch;
mod skill_loader;
#[cfg(test)]
mod watcher_test_support;

use client::adapter::AdapterPermissionGate;
pub use command_api::builtins::{runtime_build_info, BuildInfo};
use command_api::model::BuiltinCommandHandler;
use command_api::{
    parse_slash_command, CommandRegistry, CommandResult, ParsedSlashCommand,
    RegistrySlashDispatcher,
};

use command_api::builtins::{
    register_all_builtin_commands, register_core_batch_1, register_core_batch_2,
    register_core_batch_4, register_core_batch_5,
};

use lingxi_core::host::{AuthHandle, OrchestratorHandle, OutputStream};
use orchestrator::{ConversationOrchestrator, ProviderApiAdapter};
use permission::gate::PermissionGate;

use skill_api::SkillRegistry;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use tokio::sync::RwLock;
use tool_api::SessionCwd;
use tool_api::{BuiltinToolContext, ToolRegistry};
pub use tool_cron as loop_tools;

#[cfg(unix)]
use platform_posix::PosixMcpTransport;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;

#[cfg(unix)]
type DesktopMcpTransport = PosixMcpTransport;
#[cfg(windows)]
type DesktopMcpTransport = WindowsMcpTransport;

fn new_desktop_mcp_transport() -> Arc<DesktopMcpTransport> {
    Arc::new(DesktopMcpTransport::new())
}

/// Late-bound bridge from hook execution to the session's MCP registry.
///
/// Hooks are constructed before the MCP registry because the registry itself
/// needs the hook dispatcher for elicitation. The `OnceLock` breaks that
/// construction cycle. Its weak registry reference avoids a runtime ownership
/// cycle without permitting hooks to discover or connect servers:
/// invocation only uses `McpRegistry::get_client`, the already-live lookup.
#[derive(Clone, Default)]
struct DesktopHookMcpInvoker {
    registry: Arc<OnceLock<std::sync::Weak<mcp::McpRegistry>>>,
}

impl DesktopHookMcpInvoker {
    fn bind(&self, registry: Arc<mcp::McpRegistry>) {
        let _ = self.registry.set(Arc::downgrade(&registry));
    }
}

fn hook_mcp_full_name(server: &str, tool: &str) -> String {
    if tool.starts_with("mcp__") {
        tool.to_string()
    } else {
        format!(
            "mcp__{}__{}",
            mcp::normalization::normalize_name_for_mcp(server),
            tool
        )
    }
}

fn mcp_hook_text_content(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .flat_map(mcp_hook_text_content)
            .collect::<Vec<_>>(),
        serde_json::Value::Object(object)
            if object.get("type").and_then(serde_json::Value::as_str) == Some("text") =>
        {
            object
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(|text| vec![text.to_string()])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn map_hook_mcp_tool_result(
    result: lingxi_core::host::McpToolResultDto,
) -> hooks::HookMcpInvocationResult {
    let text_content = mcp_hook_text_content(&result.content);
    if result.is_error {
        hooks::HookMcpInvocationResult::Error {
            message: text_content
                .first()
                .cloned()
                .unwrap_or_else(|| "MCP tool returned isError".to_string()),
            text_content,
        }
    } else {
        hooks::HookMcpInvocationResult::Success { text_content }
    }
}

fn map_hook_mcp_tool_error(error: mcp::McpClientError) -> hooks::HookMcpInvocationResult {
    match error {
        mcp::McpClientError::Timeout { .. } | mcp::McpClientError::IdleTimeout { .. } => {
            hooks::HookMcpInvocationResult::Timeout {
                text_content: Vec::new(),
            }
        }
        other => hooks::HookMcpInvocationResult::Error {
            text_content: Vec::new(),
            message: other.to_string(),
        },
    }
}

#[async_trait::async_trait]
impl hooks::HookMcpInvoker for DesktopHookMcpInvoker {
    async fn invoke(&self, request: hooks::HookMcpInvocation) -> hooks::HookMcpInvocationResult {
        let Some(registry) = self.registry.get().and_then(std::sync::Weak::upgrade) else {
            return hooks::HookMcpInvocationResult::NotConnected {
                message: "MCP registry is not ready".to_string(),
            };
        };
        let Some(client) = registry.get_client(&request.server).await else {
            return hooks::HookMcpInvocationResult::NotConnected {
                message: format!("MCP server {:?} is not connected", request.server),
            };
        };

        let full_name = hook_mcp_full_name(&request.server, &request.tool);
        let input = serde_json::Value::Object(request.input.into_iter().collect());
        match client
            .call_tool_with_timeout(&full_name, input, request.timeout)
            .await
        {
            Ok(result) => map_hook_mcp_tool_result(result),
            Err(error) => map_hook_mcp_tool_error(error),
        }
    }
}

#[cfg(test)]
#[path = "tests/desktop_hook_mcp_invoker_tests.rs"]
mod desktop_hook_mcp_invoker_tests;

#[cfg(test)]
#[path = "tests/mcp_transport_wiring_tests.rs"]
mod mcp_transport_wiring_tests;

struct DesktopWebSearchConfigProvider {
    lingxi_home: std::path::PathBuf,
    credentials: Arc<secret::CredentialManager>,
}

#[async_trait::async_trait]
impl lingxi_core::host::WebSearchConfigProvider for DesktopWebSearchConfigProvider {
    async fn load_web_search_config(&self) -> lingxi_core::host::WebSearchRuntimeConfig {
        let settings_path = self.lingxi_home.join("settings.json");
        let parsed = std::fs::read_to_string(&settings_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .map(|v| tool_web::web_search_config::WebSearchConfig::from_settings_json(&v))
            .unwrap_or_default();
        let tavily_key = self
            .credentials
            .get_provider_key("web:tavily")
            .await
            .ok()
            .flatten()
            .map(|s| s.expose_secret().clone());
        let brave_key = self
            .credentials
            .get_provider_key("web:brave")
            .await
            .ok()
            .flatten()
            .map(|s| s.expose_secret().clone());
        lingxi_core::host::WebSearchRuntimeConfig {
            provider: Some(parsed.provider.as_str().to_string()),
            searxng_url: parsed.searxng_url,
            tavily_key,
            brave_key,
        }
    }
}

async fn emit_mcp_servers_inventory(
    bus: &telemetry::AnalyticsBus,
    payload: &telemetry::tengu::mcp::ServersPayload,
) {
    let mut metadata = telemetry::LogEventMetadata::new();
    for (key, value) in [
        ("enterprise", payload.enterprise),
        ("global", payload.global),
        ("project", payload.project),
        ("user", payload.user),
        ("plugin", payload.plugin),
        ("agent", payload.agent),
        ("claudeai", payload.claudeai),
    ] {
        metadata.insert(
            key.to_string(),
            telemetry::AnalyticsValue::Int(i64::from(value)),
        );
    }
    bus.log_event(telemetry::tengu::mcp::SERVERS, metadata)
        .await;
}

async fn emit_mcp_tools_commands_loaded(
    bus: &telemetry::AnalyticsBus,
    payload: &telemetry::tengu::mcp::ToolsCommandsLoadedPayload,
) {
    let metadata = [
        (
            "tools_count".to_string(),
            telemetry::AnalyticsValue::Int(i64::from(payload.tools_count)),
        ),
        (
            "commands_count".to_string(),
            telemetry::AnalyticsValue::Int(i64::from(payload.commands_count)),
        ),
        (
            "commands_metadata_length".to_string(),
            telemetry::AnalyticsValue::Int(i64::from(payload.commands_metadata_length)),
        ),
    ]
    .into_iter()
    .collect();
    bus.log_event(telemetry::tengu::mcp::TOOLS_COMMANDS_LOADED, metadata)
        .await;
}

fn utf16_code_units_len(value: &str) -> u32 {
    u32::try_from(value.encode_utf16().count()).unwrap_or(u32::MAX)
}

fn registered_mcp_tool_count(
    tools: &[(
        lingxi_core::types::McpConnectionId,
        Vec<Arc<dyn tool_api::Tool>>,
    )],
) -> u32 {
    tools.iter().fold(0u32, |total, (_, tools)| {
        total.saturating_add(u32::try_from(tools.len()).unwrap_or(u32::MAX))
    })
}

fn mcp_servers_inventory_payload(
    configs: &[mcp::McpServerConfig],
) -> telemetry::tengu::mcp::ServersPayload {
    let mut by_name = BTreeMap::new();
    for config in configs {
        let bucket = match config.scope {
            // LingXi's `Managed` scope is a port-side split of the same
            // enterprise-managed settings tier. The 2.1.252 oracle has no
            // separate `managed` inventory bucket.
            mcp::ConfigScope::Enterprise
            | mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed) => {
                "enterprise"
            }
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User) => "global",
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Project) => "project",
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Local) => "user",
            mcp::ConfigScope::Dynamic => "plugin",
            mcp::ConfigScope::Agent => "agent",
            mcp::ConfigScope::ClaudeAi => "claudeai",
        };
        by_name.insert(config.name.clone(), bucket);
    }

    let mut payload = telemetry::tengu::mcp::ServersPayload {
        enterprise: 0,
        global: 0,
        project: 0,
        user: 0,
        plugin: 0,
        agent: 0,
        claudeai: 0,
    };
    for bucket in by_name.values() {
        match *bucket {
            "enterprise" => payload.enterprise = payload.enterprise.saturating_add(1),
            "global" => payload.global = payload.global.saturating_add(1),
            "project" => payload.project = payload.project.saturating_add(1),
            "user" => payload.user = payload.user.saturating_add(1),
            "plugin" => payload.plugin = payload.plugin.saturating_add(1),
            "agent" => payload.agent = payload.agent.saturating_add(1),
            "claudeai" => payload.claudeai = payload.claudeai.saturating_add(1),
            _ => {}
        }
    }
    payload
}

fn mcp_tools_commands_loaded_payload(
    tools_count: u32,
    commands: &[command_api::model::SlashCommand],
) -> telemetry::tengu::mcp::ToolsCommandsLoadedPayload {
    let commands_metadata_length = commands.iter().fold(0u32, |total, command| {
        total
            .saturating_add(utf16_code_units_len(&command.name))
            .saturating_add(utf16_code_units_len(&command.description))
            .saturating_add(
                command
                    .argument_hint
                    .as_deref()
                    .map_or(0, utf16_code_units_len),
            )
    });
    telemetry::tengu::mcp::ToolsCommandsLoadedPayload {
        tools_count,
        commands_count: u32::try_from(commands.len()).unwrap_or(u32::MAX),
        commands_metadata_length,
    }
}

#[cfg(test)]
#[path = "tests/mcp_telemetry_helper_tests.rs"]
mod mcp_telemetry_helper_tests;

/// M10 (T13): per-teammate `StateMachinePool` slot cap.
///
/// Teammates are PERSISTENT: each one parks on `wait_for_message` between
/// turn-sets and NEVER frees its pool slot until killed. Sharing the
/// `AgentTool` `subagent_pool` (cap 4) would let parked teammates starve
/// one-shot subagent spawns, so the teammate handler gets its OWN pool with
/// this cap (T14 adds the pool-starvation regression that proves the
/// separation). Sized to match the `subagent_pool` cap so a coordinator can run
/// a small team without immediately exhausting slots.
///
/// `pub` so the T14 `pool_starvation` regression test can pin its parked-teammate
/// count to the single production source of truth (no magic-number drift).
pub const TEAMMATE_POOL_CAP: usize = 4;

/// Create the private root for an ephemeral host's spend ledger.
///
/// `--no-session-persistence` promises no transcript, not unaccounted spend.
/// The ledger lives under the OS temp root, is owner-only, and is removed by
/// the shutdown barrier; a killed process leaves at most one directory the OS
/// reclaims on its own.
fn ephemeral_session_home() -> Result<std::path::PathBuf, BuildError> {
    let name = format!(
        "lingxi-ephemeral-{}-{}",
        std::process::id(),
        lingxi_core::types::SessionId::new().as_uuid().simple()
    );
    let root = std::env::temp_dir();
    lingxi_core::host::rooted_fs::ensure_private_directory(
        &root,
        std::path::Path::new(&name),
        session::jsonl::journal::SESSION_STATE_DIR_MODE,
    )
    .map_err(|error| BuildError::DurableSession(error.to_string()))?;
    Ok(root.join(name))
}

#[cfg(test)]
#[path = "tests/managed_otel_env_tests.rs"]
mod managed_otel_env_tests;

/// M10 (T13): a late-bound [`lingxi_core::host::tool_invoker::ToolInvoker`] resolving the
/// composition-root construction cycle.
///
/// The teammate handler is registered into the `TaskRegistry` (which needs
/// `&mut self`, so BEFORE the registry is `Arc`-wrapped) yet must inherit the
/// parent's `Arc<ToolRegistry>` as its tool-dispatch seam — and that registry is
/// assembled AFTER the task registry exists (its `BuiltinToolContext` carries
/// `task_registry.clone()`). Naively this is a cycle.
///
/// `DeferredToolInvoker` breaks it: it is constructed empty, injected into the
/// teammate handler up front, and [`set`](Self::set) is called exactly once with
/// the real `RegistryToolInvoker` after `tools` is built. This preserves the
/// recursion-lock invariant (the teammate dispatches through the SAME
/// `Arc<ToolRegistry>` the parent owns — `RegistryToolInvoker` stores that Arc
/// verbatim) while satisfying the construction order. A teammate cannot dispatch
/// a tool before `build()` returns, so the cell is always filled before first
/// use.
struct DeferredToolInvoker {
    inner: std::sync::OnceLock<Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>>,
}

impl DeferredToolInvoker {
    fn new() -> Self {
        Self {
            inner: std::sync::OnceLock::new(),
        }
    }

    /// Fill the cell with the real invoker. Idempotent-safe: a second call is a
    /// no-op (the first binding wins), matching the build-once semantics.
    fn set(&self, invoker: Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>) {
        let _ = self.inner.set(invoker);
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::tool_invoker::ToolInvoker for DeferredToolInvoker {
    async fn invoke_detailed(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<
        lingxi_core::host::tool_invoker::ToolInvocationResult,
        lingxi_core::host::tool_invoker::ToolInvokerError,
    > {
        match self.inner.get() {
            Some(invoker) => {
                invoker
                    .invoke_detailed(name, input, ctx, workspace_lease_token)
                    .await
            }
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    async fn invoke(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        match self.inner.get() {
            Some(invoker) => invoker.invoke(name, input, ctx).await,
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    /// Forward the lease token instead of inheriting the trait's delegating
    /// default. This wrapper sits between the lease PRODUCER
    /// (`WorkspaceLeaseToolInvoker`) and the CONSUMER (`RegistryToolInvoker`,
    /// which folds the token into `PermissionCheckContext`), so the default —
    /// which drops the token and calls `invoke` — left
    /// `workspace_lease_token` permanently `None` in production: the lease
    /// ALLOW never fired, and neither did the paired `denies_host_owned_for_token`
    /// hard deny.
    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
        workspace_lease_token: Option<u64>,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        match self.inner.get() {
            Some(invoker) => {
                invoker
                    .invoke_with_workspace_lease(name, input, ctx, workspace_lease_token)
                    .await
            }
            None => Err(lingxi_core::host::tool_invoker::ToolInvokerError::Internal(
                "DeferredToolInvoker: tool dispatch attempted before build() bound the registry"
                    .to_string(),
            )),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Late-bound teammate system-prompt renderer. The weak reference avoids the
/// cycle `orchestrator -> tools -> task registry -> teammate handler ->
/// orchestrator` while still rebuilding dynamic cwd/git/memory sections for
/// every teammate spawn.
struct OrchestratorTeammatePromptRenderer {
    orchestrator: std::sync::Weak<ConversationOrchestrator>,
}

/// Resolve the coordinator worker's declared `agent_type` against the live
/// catalog. The spawn seam only carries the worker id + display name, so the
/// team registry is the authoritative bridge between those identities. Unknown
/// types retain the permissive built-in fallback.
struct CoordinatorTeammateDefinitionResolver {
    team: Arc<coordinator::TeamRegistry>,
    catalog: Arc<RwLock<Vec<agent::AgentDefinition>>>,
}

#[async_trait::async_trait]
impl tasks::handlers::TeammateDefinitionResolver for CoordinatorTeammateDefinitionResolver {
    async fn resolve(
        &self,
        agent_id: &lingxi_core::types::AgentId,
        display_name: &str,
    ) -> Option<agent::AgentDefinition> {
        let agent_type = self
            .team
            .find_by_agent_id(agent_id)
            .await
            .map_or_else(|| display_name.to_string(), |worker| worker.agent_type);
        if let Some(definition) = self
            .catalog
            .read()
            .await
            .iter()
            .find(|definition| definition.agent_type == agent_type)
            .cloned()
        {
            return Some(definition);
        }
        tasks::handlers::TeammateDefinitionResolver::resolve(
            &tasks::handlers::DefaultTeammateDefinition,
            agent_id,
            &agent_type,
        )
        .await
    }
}

#[async_trait::async_trait]
impl tasks::handlers::TeammateSystemPromptRenderer for OrchestratorTeammatePromptRenderer {
    async fn render_default_system_prompt(&self) -> String {
        match self.orchestrator.upgrade() {
            Some(orchestrator) => orchestrator.assemble_default_system_prompt_preview().await,
            None => String::new(),
        }
    }
}

/// Fan out teammate status transitions to both the durable task row and the
/// coordinator's worker registry. The task-row leg is load-bearing for
/// `TeamSpawnSeam::is_alive`; without it a terminal teammate leaves its mailbox
/// pump believing the retained task row is still Running.
struct TeammateStatusFanout {
    task_registry: Arc<tasks::registry_status_sink::RegistryStatusSink>,
    coordinator: Arc<coordinator::CoordinatorStatusSink>,
}

#[async_trait::async_trait]
impl tasks::handlers::TaskStatusSink for TeammateStatusFanout {
    fn requires_explicit_activation(&self) -> bool {
        tasks::handlers::TaskStatusSink::requires_explicit_activation(self.task_registry.as_ref())
    }

    async fn set_status(&self, task_id: &str, status: tasks::TaskStatus) {
        tasks::handlers::TaskStatusSink::set_status(self.task_registry.as_ref(), task_id, status)
            .await;
        tasks::handlers::TaskStatusSink::set_status(self.coordinator.as_ref(), task_id, status)
            .await;
    }

    async fn set_teammate_idle(&self, task_id: &str) {
        tasks::handlers::TaskStatusSink::set_teammate_idle(self.task_registry.as_ref(), task_id)
            .await;
        tasks::handlers::TaskStatusSink::set_teammate_idle(self.coordinator.as_ref(), task_id)
            .await;
    }

    async fn set_awaiting_plan_approval(&self, task_id: &str, awaiting: bool) {
        tasks::handlers::TaskStatusSink::set_awaiting_plan_approval(
            self.task_registry.as_ref(),
            task_id,
            awaiting,
        )
        .await;
        tasks::handlers::TaskStatusSink::set_awaiting_plan_approval(
            self.coordinator.as_ref(),
            task_id,
            awaiting,
        )
        .await;
    }

    async fn set_failed(&self, task_id: &str, error: &str) {
        tasks::handlers::TaskStatusSink::set_failed(self.task_registry.as_ref(), task_id, error)
            .await;
        tasks::handlers::TaskStatusSink::set_failed(self.coordinator.as_ref(), task_id, error)
            .await;
    }

    // ── The remaining `TaskStatusSink` surface ──────────────────────────────
    //
    // `RegistryStatusSink` overrides every method below; `CoordinatorStatusSink`
    // takes the trait default for all of them. Leaving them unimplemented here
    // would silently swap the registry's real answers for the trait defaults
    // (`is_registered` ⇒ `true`, `is_terminal` ⇒ `false`) the moment the
    // teammate handler starts calling them — a decorator that forgets what the
    // decorated type already did. Side-effecting methods fan out to both legs;
    // the two lifecycle QUERIES resolve against the durable task row, which is
    // the only leg that stores one.

    async fn set_exit_code(&self, task_id: &str, exit_code: i32) {
        tasks::handlers::TaskStatusSink::set_exit_code(
            self.task_registry.as_ref(),
            task_id,
            exit_code,
        )
        .await;
        tasks::handlers::TaskStatusSink::set_exit_code(
            self.coordinator.as_ref(),
            task_id,
            exit_code,
        )
        .await;
    }

    async fn set_pid(&self, task_id: &str, pid: u32) {
        tasks::handlers::TaskStatusSink::set_pid(self.task_registry.as_ref(), task_id, pid).await;
        tasks::handlers::TaskStatusSink::set_pid(self.coordinator.as_ref(), task_id, pid).await;
    }

    async fn notify_rest(
        &self,
        task_id: &str,
        result: Option<String>,
        usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
        agent_id: Option<lingxi_core::types::AgentId>,
        agent_name: Option<String>,
        team_name: Option<String>,
    ) {
        tasks::handlers::TaskStatusSink::notify_rest(
            self.task_registry.as_ref(),
            task_id,
            result.clone(),
            usage.clone(),
            agent_id,
            agent_name.clone(),
            team_name.clone(),
        )
        .await;
        tasks::handlers::TaskStatusSink::notify_rest(
            self.coordinator.as_ref(),
            task_id,
            result,
            usage,
            agent_id,
            agent_name,
            team_name,
        )
        .await;
    }

    async fn set_agent_outcome(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::AgentTerminalOutcome,
    ) {
        tasks::handlers::TaskStatusSink::set_agent_outcome(
            self.task_registry.as_ref(),
            task_id,
            outcome.clone(),
        )
        .await;
        tasks::handlers::TaskStatusSink::set_agent_outcome(
            self.coordinator.as_ref(),
            task_id,
            outcome,
        )
        .await;
    }

    async fn notify_monitor_event(&self, task_id: &str, event: &str, housekeeping: bool) {
        tasks::handlers::TaskStatusSink::notify_monitor_event(
            self.task_registry.as_ref(),
            task_id,
            event,
            housekeeping,
        )
        .await;
        tasks::handlers::TaskStatusSink::notify_monitor_event(
            self.coordinator.as_ref(),
            task_id,
            event,
            housekeeping,
        )
        .await;
    }

    async fn is_registered(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_registered(self.task_registry.as_ref(), task_id).await
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_terminal(self.task_registry.as_ref(), task_id).await
    }
}

/// Session-owned routing for teammate messages. Team creation is implicit;
/// the tool registry exposes only SendMessage alongside Agent.
pub struct CoordinatorWiring {
    /// Shared per-session member registry.
    pub team: Arc<coordinator::TeamRegistry>,
    /// Backing task cancellation and message delivery.
    pub spawn_seam: Arc<dyn lingxi_core::host::team_spawn::TeamSpawnSeam>,
}

fn teammate_backend_selector(
    cwd: std::path::PathBuf,
    flag_mode: Option<lingxi_core::settings::schema::TeammateMode>,
    is_tty: bool,
) -> Arc<dyn Fn() -> pane_teammate::PaneBackendSelection + Send + Sync> {
    use platform_posix::swarm::detection::{
        detect_terminal_env, select_backend, BackendChoice, TeammateMode,
    };
    let backends = std::sync::Mutex::new(std::collections::HashMap::<
        &'static str,
        Arc<dyn lingxi_core::host::SwarmBackend>,
    >::new());
    Arc::new(move || {
        let mode = match flag_mode
            .or_else(|| load_merged_settings(&cwd).and_then(|s| s.settings.teammate_mode))
        {
            Some(lingxi_core::settings::schema::TeammateMode::InProcess) => TeammateMode::InProcess,
            Some(lingxi_core::settings::schema::TeammateMode::Tmux) => TeammateMode::Tmux,
            Some(lingxi_core::settings::schema::TeammateMode::ITerm2) => TeammateMode::ITerm2,
            _ => TeammateMode::Auto,
        };
        let terminal = detect_terminal_env();
        let interactive = is_tty && !lingxi_core::host::session_flags::is_non_interactive_session();
        let selection = select_backend(&terminal, mode, interactive, false);
        let acquisition_error = if mode == TeammateMode::Auto
            && interactive
            && (terminal.inside_tmux || terminal.iterm_app)
        {
            select_backend(&terminal, TeammateMode::Tmux, true, false)
                .err()
                .map(str::to_string)
        } else {
            None
        };
        let (backend, error) = match selection {
            Ok(BackendChoice::InProcess) => (None, acquisition_error),
            Ok(choice) => {
                let mut cache = backends
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let key = if choice == BackendChoice::Tmux {
                    "tmux"
                } else {
                    "iterm2"
                };
                let backend = cache.entry(key).or_insert_with(|| match choice {
                    BackendChoice::Tmux => Arc::new(platform_posix::swarm::TmuxBackend::new()),
                    _ => Arc::new(platform_posix::swarm::ITermSwarmBackend::new()),
                });
                (Some(backend.clone()), None)
            }
            Err(error) => (None, Some(error.to_string())),
        };
        pane_teammate::PaneBackendSelection {
            backend,
            explicit: matches!(mode, TeammateMode::Tmux | TeammateMode::ITerm2),
            error,
        }
    })
}

/// Assemble the desktop builtin **tool** registry from a freshly-built
/// [`BuiltinToolContext`].
///
/// Includes cross-platform tools and desktop Agent, worktree, MCP and LSP tools.
/// `coordinator` selects the session-backed SendMessage implementation.
/// Team lifecycle is implicit in both registry variants.
///
/// `cron_auth` is the in-process OAuth resolver `RemoteTrigger` uses; `None`
/// leaves the tool on its "not authenticated" pre-flight path (used by the
/// offline registry-snapshot tests).
#[must_use]
pub fn desktop_tool_registry(
    ctx: BuiltinToolContext,
    coordinator: Option<CoordinatorWiring>,
    cron_auth: Option<Arc<dyn tool_cron::ClaudeAiAuthProvider>>,
) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    // Offline / snapshot path: no command registry to back the Skill tool, so it
    // gets the hermetic `EmptySkillLoader` (tool name unchanged → snapshot-safe).
    // No `CwdChanged` firer here either (offline factory has no hook executor) —
    // the BashTool is the byte-identical no-firer variant. No shared live-cwd
    // cell either: every tool falls back to `ctx.workspace` / the process cwd.
    let _ = register_desktop_tools(
        &mut reg,
        ctx,
        coordinator,
        None,
        true,
        None,
        cron_auth,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    reg
}

/// Launches `LocalWorkflow` background tasks for the `Workflow` tool by spawning
/// through the shared [`tasks::registry::TaskRegistry`]. Resolves the spec's
/// `scriptPath` / `script` / `name` to a script source (claude-code precedence);
/// `scriptPath`/`name` are read from disk relative to `cwd`.
struct TaskRegistryWorkflowLauncher {
    registry: Arc<tasks::registry::TaskRegistry>,
    /// Project cwd that owns the session directory; fixed for the session.
    project_cwd: std::path::PathBuf,
    /// Live cwd shared with Bash/orchestrator and sampled for each launch.
    current_cwd: Arc<std::sync::Mutex<std::path::PathBuf>>,
    /// The claude home directory (e.g. `~/.claude`), used to derive
    /// `transcriptDir = <sessionProjectDir>/<sessionId>/subagents/workflows/<runId>`.
    lingxi_home: std::path::PathBuf,
    /// The main session UUID (bare uuid string, no `sess:` prefix), threaded
    /// from the composition root's `main_session_uuid` so the transcript dir
    /// anchors on the correct session.
    session_uuid: String,
    /// Live provider-qualified session model selection published after the
    /// orchestrator exists.
    default_model_selection_provider:
        Arc<std::sync::OnceLock<agent::handle::DefaultModelSelectionProvider>>,
    /// The SAME `workflow::PluginWorkflowRegistry` the composition root hands
    /// to `plugin::PluginManager` and `tool_workflow::WorkflowTool` (§14).
    ///
    /// It must be the same one: `WorkflowTool::validate_input` consults the
    /// registry to decide whether a name resolves, and this launcher resolves
    /// the script it validated. Wiring only one of the two would make
    /// `validate_input` accept `acme:deploy` and then fail here with
    /// `Workflow "acme:deploy" not found. Available: (none)`.
    plugin_workflows: Arc<workflow::PluginWorkflowRegistry>,
}

#[async_trait::async_trait]
impl tool_workflow::WorkflowLauncher for TaskRegistryWorkflowLauncher {
    async fn launch(
        &self,
        mut spec: tool_workflow::WorkflowLaunchSpec,
    ) -> Result<tool_workflow::WorkflowLaunched, tool_workflow::WorkflowLaunchError> {
        // Tool-driven calls carry the live originating session; only legacy
        // callers that omit it should fall back to the boot session.  Keeping
        // this identity through path derivation and TaskRegistry spawn keeps
        // late workflow settlement on the session that launched it after a
        // Clear/Resume switch.
        let session_uuid = spec
            .session_uuid
            .clone()
            .map(|raw| {
                // WorkflowLaunchSpec accepts legacy `sess:<uuid>` and the
                // canonical bare UUID form. Transcript/task ownership uses
                // the latter, so normalize once at this trusted host
                // boundary rather than splitting a resumed workflow across
                // two session directories.
                lingxi_core::types::SessionId::parse_prefixed(&raw)
                    .map_or(raw, |id| id.as_uuid().to_string())
            })
            .unwrap_or_else(|| self.session_uuid.clone());
        let cwd = self
            .current_cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let abs = |p: &str| -> std::path::PathBuf {
            let path = std::path::Path::new(p);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
        };
        // A caller-supplied scriptPath must carry the exact approval snapshot
        // produced by `WorkflowTool::check_permissions`. Never reopen the
        // model pathname here: a parent or leaf symlink may have retargeted
        // since the nested Read decision.
        let script =
            if let Some(raw_path) = spec.script_path.as_deref().filter(|path| !path.is_empty()) {
                let approval = spec.script_path_approval.take().ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "Workflow scriptPath permission snapshot is missing; refusing to read it"
                            .into(),
                    )
                })?;
                let requested = abs(raw_path);
                if approval.requested != requested {
                    return Err(tool_workflow::WorkflowLaunchError(
                        "Workflow scriptPath changed after permission was checked".into(),
                    ));
                }
                tool_workflow::read_script_path_after_permission(&approval)?
            } else {
                // §14 — the SAME registry `WorkflowTool::validate_input` checked,
                // so a name that validated resolves here too.
                tool_workflow::resolve_script_at(
                    &cwd,
                    &spec,
                    |p| std::fs::read_to_string(abs(p)),
                    Some(self.plugin_workflows.as_ref()),
                )?
            };
        // Reject a malformed `meta` block at the tool boundary (claude-code parses
        // + validates `meta` when the Workflow tool accepts a script). The
        // byte-exact message surfaces to the model as the tool error.
        workflow::validate_meta(&script).map_err(|e| {
            let msg = match e {
                workflow::WorkflowError::Script(m) => m,
                other => other.to_string(),
            };
            tool_workflow::WorkflowLaunchError(msg)
        })?;
        // Determinism gate (claude-code validateInput `if (e.script && HKa(...))`):
        // an INLINE `script` may not use Date.now()/Math.random()/new Date()
        // (breaks resume). Author-controlled `scriptPath`/`name` files are exempt.
        let is_inline = spec.script.as_deref().is_some_and(|s| !s.is_empty())
            && spec
                .script_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none();
        if is_inline {
            if let Err(workflow::WorkflowError::Script(m)) = workflow::check_determinism(&script) {
                return Err(tool_workflow::WorkflowLaunchError(m));
            }
        }
        if let Err(error) = workflow::validate_body(&script) {
            let error = match error {
                workflow::WorkflowError::Engine(message)
                | workflow::WorkflowError::Script(message) => message,
            };
            let run_id = tool_workflow::mint_run_id(spec.resume_from_run_id.as_deref());
            let workflow_name = workflow::meta_string_value(&script, "name");
            let summary = workflow::meta_string_value(&script, "description");
            return Ok(tool_workflow::WorkflowLaunched {
                task_id: tasks::generate_task_id(tasks::TaskType::LocalWorkflow),
                run_id: Some(run_id),
                workflow_name,
                summary,
                error: Some(error),
                ..Default::default()
            });
        }
        // Resume gate (claude-code validateInput errorCode 3): a `resumeFromRunId`
        // that names a STILL-RUNNING workflow is rejected — two runs sharing a run
        // id would race on the same journal. The WorkflowTool can't reach the task
        // registry from its `ToolUseContext`, so the gate lives here in the
        // launcher (which owns the registry). Message byte-exact (`ED` = TaskStop).
        if let Some(rid) = spec.resume_from_run_id.as_deref().filter(|s| !s.is_empty()) {
            if !tool_workflow::is_valid_run_id(rid) {
                return Err(tool_workflow::WorkflowLaunchError(format!(
                    "resumeFromRunId {rid:?} is not a workflow run id (expected wf_ followed by \
                     at least 6 lowercase alphanumerics or dashes)"
                )));
            }
            if let Some(task_id) = self.registry.find_running_workflow_by_run_id(rid).await {
                return Err(tool_workflow::WorkflowLaunchError(format!(
                    "Workflow {rid} is still running (task {task_id}). Stop it first with \
                     TaskStop({{taskId: \"{task_id}\"}}) before resuming."
                )));
            }
        }

        // Mint the run id at launch (fresh) or reuse the resume id — so it can be
        // returned in the tool result (claude-code `runId`) for `resumeFromRunId`.
        // A clock-nanos × per-process sequence gives a unique id (host clock use
        // is fine — only the workflow SCRIPT is barred from the clock). The
        // surfaced shape matches claude-code 2.1.195 `wf_${randomUUID().slice(0,12)}`
        // = `wf_` + 8 hex + `-` + 3 hex (the first 12 chars of a v4 UUID).
        let run_id = tool_workflow::mint_run_id(spec.resume_from_run_id.as_deref());
        // `meta.name` → `workflowName` in the result.
        let workflow_name = workflow::meta_string_value(&script, "name");
        // `meta.description` → `summary` in the result (claude-code `p = c.meta.description`).
        let summary = workflow::meta_string_value(&script, "description");
        let task_description = summary
            .clone()
            .unwrap_or_else(|| "Dynamic workflow".to_string());
        // Reserve before the first run-id-derived filesystem write. Collect
        // every subsequent error in one block so release is unconditional.
        let reservation = self
            .registry
            .try_reserve_workflow_run_id(&run_id)
            .await
            .map_err(|error| tool_workflow::WorkflowLaunchError(error.to_string()))?;
        let launch_result = async {
            // Persist the script so it is editable + re-runnable via `scriptPath`
            // (claude-code persists every invocation's script "under the session
            // directory"). A `scriptPath` input is already on disk → return it as-is;
            // an inline/`name` script is written under the session's workflow directory.
            let subagents = orchestrator::transcript_paths::subagents_dir(
                &self.lingxi_home,
                &self.project_cwd.to_string_lossy(),
                &session_uuid,
            );
            let script_path = if let Some(p) = spec.script_path.as_deref().filter(|s| !s.is_empty())
            {
                abs(p).to_str().map(str::to_string).ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "workflow script path is not valid UTF-8".to_string(),
                    )
                })?
            } else {
                let session_dir = subagents.parent().ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "cannot derive workflow session directory".to_string(),
                    )
                })?;
                let dir = session_dir.join("workflows");
                let file = dir.join(format!("{run_id}.js"));
                std::fs::create_dir_all(&dir).map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot create workflow script directory '{}': {error}",
                        dir.display()
                    ))
                })?;
                std::fs::write(&file, &script).map_err(|error| {
                    tool_workflow::WorkflowLaunchError(format!(
                        "cannot persist workflow script '{}': {error}",
                        file.display()
                    ))
                })?;
                file.to_str().map(str::to_string).ok_or_else(|| {
                    tool_workflow::WorkflowLaunchError(
                        "workflow script path is not valid UTF-8".to_string(),
                    )
                })?
            };
            // `transcriptDir` = `<sessionProjectDir>/<sessionId>/subagents/workflows/<runId>`
            // (claude-code `Nte(runId)` → `path.join(CU() ?? _g(gr()), xt(), "subagents",
            // "workflows", e)`). We derive via `orchestrator::transcript_paths::subagents_dir`
            // which computes `<lingxi_home>/projects/<sanitize(cwd)>/<session_uuid>/subagents`,
            // then append `workflows/<runId>`.
            let transcript_dir = { subagents.join("workflows").join(&run_id) };
            std::fs::create_dir_all(&transcript_dir).map_err(|error| {
                tool_workflow::WorkflowLaunchError(format!(
                    "cannot create workflow transcript directory '{}': {error}",
                    transcript_dir.display()
                ))
            })?;
            let transcript_dir_wire = transcript_dir.to_str().map(str::to_string);
            // Derive telemetry fields for tengu_workflow_launched (oracle §7).
            // Claude keeps launch origin separate from the verbatim-builtin
            // flag: an arbitrary scriptPath is still sourced as `scriptPath`,
            // while a named workflow is builtin only when the resolver chose
            // that exact bundled name and source.
            let has_script_path = spec
                .script_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_some();
            let named_source = spec
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|name| {
                    tool_workflow::workflow_source_for_name(
                        &cwd,
                        name,
                        Some(self.plugin_workflows.as_ref()),
                    )
                });
            let named_builtin = spec
                .name
                .as_deref()
                .and_then(|name| tool_workflow::BUILTIN_WORKFLOWS.get(name))
                .is_some_and(|descriptor| descriptor.script == script);
            let has_name = spec.name.as_deref().filter(|s| !s.is_empty()).is_some();
            // Claude's invocation mode uses scriptPath first, then the named
            // selector, then a standalone inline script. Keep telemetry's
            // origin in that order when callers provide multiple selectors;
            // a named call may still carry an explicit script body.
            let (invocation_mode, workflow_source) = if has_script_path {
                ("scriptPath".to_string(), "scriptPath".to_string())
            } else if has_name {
                (
                    "named".to_string(),
                    if named_builtin {
                        "built-in".to_string()
                    } else {
                        named_source.unwrap_or("custom").to_string()
                    },
                )
            } else {
                ("inline".to_string(), "inline".to_string())
            };
            let task_id = self
                .registry
                .spawn(
                    tasks::TaskType::LocalWorkflow,
                    tasks::TaskSpawnInput::LocalWorkflow {
                        session_uuid: Some(session_uuid.clone()),
                        // Display name = the script's `meta.name` (claude-code
                        // `workflowName`), so an INLINE workflow shows its real name
                        // in `/workflows` rather than the empty fallback; a named
                        // workflow falls back to its saved `spec.name`.
                        workflow_id: workflow_name
                            .clone()
                            .filter(|s| !s.is_empty())
                            .or_else(|| spec.name.clone())
                            .unwrap_or_default(),
                        script,
                        resume_from_run_id: spec.resume_from_run_id.clone(),
                        // The `args` global, serialised to a JSON string for the runtime.
                        args: spec
                            .args
                            .as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default()),
                        run_id: Some(run_id.clone()),
                        invocation_mode: Some(invocation_mode),
                        workflow_source: Some(workflow_source),
                        script_is_verbatim_builtin: Some(named_builtin),
                        transcript_subdir: Some(transcript_dir.clone()),
                        // `t.agentId != null` in claude-code: the Workflow tool stamps
                        // the invocation source before handing the launch spec off.
                        launched_from_subagent: spec.launched_from_subagent,
                        tool_use_id: spec.tool_use_id.clone(),
                        creator_teammate_name: spec.creator_teammate_name.clone(),
                        creator_team_name: spec.creator_team_name.clone(),
                        creator_agent_id: spec
                            .creator_agent_id
                            .as_deref()
                            .and_then(lingxi_core::types::AgentId::parse_prefixed),
                        // Desktop hosts no Local Apps: no app store and no
                        // delete guard, so there is nothing for a scope to
                        // authorize. `None` rather than a purpose invented at
                        // the call site.
                        //
                        // ⚠️ Desktop DOES have a workspace-lease registry —
                        // `with_workspace_permission_leases` is wired further
                        // down this file. `None` is still right, and strictly
                        // safer: a lease is an ALLOW grant, so an unscoped
                        // desktop workflow gets less than before, never more.
                        //
                        // Unremarked behaviour delta, recorded here because the
                        // diff does not otherwise say it: before the scope was
                        // threaded, a desktop launch named like a Local App
                        // build workflow but carrying no `args.app_id` failed
                        // hard with `requires a non-empty workflow args.app_id`.
                        // It now runs silently unscoped. Nothing on desktop
                        // relies on that refusal today — there is no app store
                        // for it to protect — but a future reader looking for
                        // where it went should find this.
                        scope: None,
                    },
                    task_description,
                )
                .await
                .map_err(|e| tool_workflow::WorkflowLaunchError(e.to_string()))?;
            self.registry
                .set_workflow_resume_metadata(&task_id, script_path.clone(), transcript_dir.clone())
                .await
                .map_err(|error| tool_workflow::WorkflowLaunchError(error.to_string()))?;
            Ok(tool_workflow::WorkflowLaunched {
                task_id,
                run_id: Some(run_id.clone()),
                script_path: Some(script_path),
                workflow_name,
                summary,
                transcript_dir: transcript_dir_wire,
                error: None,
            })
        }
        .await;
        drop(reservation);
        launch_result
    }
}

/// Composition-root source of the `Stop` / `SubagentStop` hook
/// `background_tasks` + `session_crons` snapshot (claude-code
/// `Lic(taskRegistry.all())` / `Mic()`), bound to the live task registry + the
/// project-root cron file. Mirrors the [`orchestrator::RegistryTaskNotifications`]
/// precedent: it owns the SAME `Arc<dyn TaskRegistryHandle>` the tool context
/// holds, plus the shared `current_cwd` cell, and maps both sources through the
/// orchestrator's pure `build_background_tasks` / `build_session_crons` builders.
struct RegistryStopHookSnapshot {
    registry: Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    /// Cron storage is anchored to the session's project root. A Bash `cd`
    /// changes hook payload cwd, but must not silently switch which project's
    /// durable schedules appear in Stop hooks.
    project_root: std::path::PathBuf,
}

#[async_trait::async_trait]
impl orchestrator::StopHookSnapshotProvider for RegistryStopHookSnapshot {
    async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
        // claude passes `taskRegistry.all()` (NOT `.running()`); `wA` inside the
        // builder does the running|pending + isBackgrounded filtering. A registry
        // error degrades to "no tasks" so a transient failure never breaks the
        // turn.
        let records = self
            .registry
            .list(lingxi_core::host::task_registry::TaskListFilter::default())
            .await
            .unwrap_or_default();
        orchestrator::build_background_tasks(&records)
    }

    async fn background_tasks_with_start_times(
        &self,
    ) -> (
        Vec<hooks::HookBackgroundTask>,
        std::collections::HashMap<String, u64>,
    ) {
        let records = self
            .registry
            .list(lingxi_core::host::task_registry::TaskListFilter::default())
            .await
            .unwrap_or_default();
        let start_times = records
            .iter()
            .filter_map(|record| record.started_at_ms.map(|ms| (record.task_id.clone(), ms)))
            .collect();
        (orchestrator::build_background_tasks(&records), start_times)
    }

    async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
        // claude `Cv()` is the in-memory session cron list; the port persists the
        // durable cron jobs to `<project_root>/.claude/scheduled_tasks.json` —
        // the SESSION store `CronCreate`/`write_tasks_body` write, not the v2
        // task-center store beside it. Read + parse it (a missing/garbage file ⇒
        // no crons, matching claude's unreadable-file-as-empty contract) and map
        // each task into the builder's neutral input.
        let path = cron::tasks_file::session_scheduled_tasks_path(&self.project_root);
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let doc = cron::tasks_file::parse_tasks(&body);
        let mut inputs: Vec<orchestrator::CronSnapshotInput> = doc
            .tasks
            .into_iter()
            .map(|t| orchestrator::CronSnapshotInput {
                id: t.id,
                cron: t.cron,
                recurring: t.recurring,
                prompt: t.prompt,
            })
            .collect();
        if let Ok(session_jobs) = cron::session_jobs(&self.registry).await {
            inputs.extend(
                session_jobs
                    .into_iter()
                    .map(|task| orchestrator::CronSnapshotInput {
                        id: task.id,
                        cron: task.cron,
                        recurring: Some(task.recurring),
                        prompt: task.prompt,
                    }),
            );
        }
        orchestrator::build_session_crons(&inputs)
    }
}

/// Register the desktop tool set into an existing (empty) registry.
///
/// Each `tool_*::register_all` consumes a clone of `ctx`; the final crate
/// takes ownership to avoid a redundant clone.
///
/// When `coordinator` is present, its session-backed SendMessage replaces
/// the builtin implementation. Each tool name is registered exactly once.
/// JSONL-backed [`tool_api::WorktreeStatePersister`] (parity 2.1.212's
/// `saveWorktreeState`): appends a `worktree-state` entry — carrying the active
/// worktree's serialized session, or `null` on exit — to the session transcript,
/// keyed by the bare `session_uuid` the resume loader reads back
/// (`read_worktree_state`). Persistence is best-effort: a failed append is logged
/// and swallowed so it never fails the `EnterWorktree`/`ExitWorktree` tool call
/// (matching claude's `.catch(...)` on `xX`).
struct JsonlWorktreeStatePersister {
    writer: Arc<session::jsonl::writer::JsonlWriter>,
    session_uuid: String,
}

#[async_trait::async_trait]
impl tool_api::WorktreeStatePersister for JsonlWorktreeStatePersister {
    async fn persist_worktree_state(&self, session: Option<&tool_api::WorktreeSession>) {
        let payload = session.map(tool_api::WorktreeSession::to_persisted_json);
        if let Err(e) = self
            .writer
            .append_worktree_state(&self.session_uuid, payload.as_ref())
            .await
        {
            tracing::warn!(error = %e, "failed to persist worktree-state transcript record");
        }
    }
}

#[cfg(test)]
#[path = "tests/desktop_fusion_price_book_test.rs"]
mod desktop_fusion_price_book_test;

#[cfg(test)]
#[path = "tests/desktop_fusion_catalog_row_test.rs"]
mod desktop_fusion_catalog_row_test;

fn desktop_fusion_executor(
    spawner: Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawner>,
    side_query: Arc<dyn sidequery::SideQueryClient>,
    cfg: &DesktopConfig,
    attempts: Arc<fusion_attempts::DesktopFusionAttempts>,
    // Round-4 review finding [8]: a `ModelSource` (typically
    // `FusionCatalogModelSource`, `LlmStack::fusion_catalog_source`) instead
    // of a frozen `Vec<CatalogModel>` — the orchestrator already re-queries
    // `list()` on every run, so this is what lets a mid-session credential
    // refresh (`FusionCatalogRefresher::refresh`) actually reach it.
    catalog: Arc<dyn fusion::ModelSource>,
    bus: Arc<telemetry::AnalyticsBus>,
    pricing: Arc<cost::PricingCatalog>,
    implement_host: Option<Arc<dyn lingxi_core::host::FusionImplementHost>>,
) -> Arc<dyn lingxi_core::host::FusionExecutor> {
    // Boot-time validation: surface the FIRST invalid `fusion.*` value
    // through a log line, but always build the live orchestrator below —
    // its `config_source` (and `DesktopFusionExecutor::preflight_error`)
    // re-validate on every subsequent call, so a still-broken file keeps
    // failing with an up-to-date message while a fixed-then-saved one
    // recovers without a restart (finding [14]).
    if let Err(error) = desktop_fusion_runtime_config(cfg) {
        tracing::warn!(
            error = %error,
            "fusion.* settings failed boot-time validation; \
             /fusion will report this until the settings file is fixed"
        );
    }
    let config_source: Arc<dyn fusion::FusionConfigSource> =
        Arc::new(DesktopFusionConfigSource { cfg: cfg.clone() });
    let mut inner = fusion::FusionOrchestrator::new(spawner, side_query, config_source, catalog)
        .with_bus(bus)
        .with_price_book(Arc::new(DesktopFusionPriceBook::new(pricing)))
        .with_panel_admission()
        .with_attempt_registrar(attempts);
    if let Some(host) = implement_host {
        inner = inner.with_implement_host(host);
    }
    Arc::new(DesktopFusionExecutor {
        inner: Arc::new(inner),
        cfg: cfg.clone(),
    })
}

#[cfg(test)]
#[path = "tests/desktop_fusion_executor_boot_test.rs"]
mod desktop_fusion_executor_boot_test;

/// Assemble the desktop builtin tool set.
///
/// `fusion` is the live Fusion orchestrator. The offline snapshot path passes
/// `None` so the Agent listing stays inert and the locked tool-name snapshot
/// is unchanged. Mobile never reaches this function.
#[allow(clippy::too_many_arguments)]
pub fn register_desktop_tools(
    reg: &mut ToolRegistry,
    ctx: BuiltinToolContext,
    coordinator: Option<CoordinatorWiring>,
    ask_user_question_resolver: Option<
        Arc<dyn tool_ui::ask_user_question::AskUserQuestionResolver>,
    >,
    advertise_ask_user_question: bool,
    computer_access_resolver: Option<Arc<dyn tool_computer_use::ComputerAccessResolver>>,
    cron_auth: Option<Arc<dyn tool_cron::ClaudeAiAuthProvider>>,
    skill_loader: Option<Arc<dyn tool_skill::skill::SkillLoader>>,
    cwd_changed_firer: hooks::OptionalCwdChangedFirer,
    web_side_query: Option<Arc<dyn sidequery::SideQueryClient>>,
    live_cwd: Option<tool_api::LiveCwdCell>,
    worktree_state_persister: Option<Arc<dyn tool_api::WorktreeStatePersister>>,
    fusion: Option<Arc<dyn lingxi_core::host::FusionExecutor>>,
) -> (
    tool_cron::WakeupSchedulerCell,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    register_desktop_tools_with_fusion_recorder(
        reg,
        ctx,
        coordinator,
        ask_user_question_resolver,
        advertise_ask_user_question,
        computer_access_resolver,
        cron_auth,
        skill_loader,
        cwd_changed_firer,
        web_side_query,
        live_cwd,
        worktree_state_persister,
        fusion,
        None,
        None,
    )
}

/// Assemble the desktop tools with the optional host-owned Fusion recorder.
#[allow(clippy::too_many_arguments)]
pub fn register_desktop_tools_with_fusion_recorder(
    reg: &mut ToolRegistry,
    ctx: BuiltinToolContext,
    coordinator: Option<CoordinatorWiring>,
    ask_user_question_resolver: Option<
        Arc<dyn tool_ui::ask_user_question::AskUserQuestionResolver>,
    >,
    advertise_ask_user_question: bool,
    computer_access_resolver: Option<Arc<dyn tool_computer_use::ComputerAccessResolver>>,
    cron_auth: Option<Arc<dyn tool_cron::ClaudeAiAuthProvider>>,
    skill_loader: Option<Arc<dyn tool_skill::skill::SkillLoader>>,
    cwd_changed_firer: hooks::OptionalCwdChangedFirer,
    web_side_query: Option<Arc<dyn sidequery::SideQueryClient>>,
    live_cwd: Option<tool_api::LiveCwdCell>,
    worktree_state_persister: Option<Arc<dyn tool_api::WorktreeStatePersister>>,
    fusion: Option<Arc<dyn lingxi_core::host::FusionExecutor>>,
    fusion_recorder: Option<Arc<dyn lingxi_core::host::FusionRunRecorder>>,
    fusion_recorder_factory: Option<Arc<dyn lingxi_core::host::FusionRunRecorderFactory>>,
) -> (
    tool_cron::WakeupSchedulerCell,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // ----- cross-platform tool crates (also linked by harness-runtime::mobile, P11) ---
    // (P2-08) The shared live-cwd cell (`getCwd()`/`Ct()`): the desktop `BashTool`
    // writes it on a `cd`, and Read/Glob/Grep + the LSP tool read it as their live
    // cwd (default search dir, "does not exist" cwd notes, relative path root),
    // 1:1 with claude-code's single session-global cwd. `None` (offline factory)
    // falls every tool back to `ctx.workspace` / the process cwd — byte-identical.
    tool_file::register_all_with_live_cwd(reg, ctx.clone(), live_cwd.clone());
    // BASH.4 `onCwdChangedForHooks` (Shell.ts:409): when a firer is supplied (real
    // desktop sessions wire one over the shared `Arc<HookExecutorImpl>`), a `cd`
    // inside a Bash call fires the `CwdChanged` hook. `None` (the offline
    // registry-snapshot path) keeps the byte-identical no-firer BashTool — the
    // registered tool NAMES are unchanged either way, so the locked tool-list
    // snapshot is unaffected. `harness-runtime::mobile` never reaches this call (it does
    // not register the shell tools).
    tool_shell::register_all_with_cwd_firer(reg, ctx.clone(), cwd_changed_firer, live_cwd.clone());
    tool_web::register_all(reg, ctx.clone(), web_side_query);
    tool_plan::register_all(reg, ctx.clone());
    tool_meta::register_all(reg, ctx.clone());
    // The `computer` tool (M8-P11b). Cross-platform-registerable — its own
    // `is_enabled()` gates on `ctx.computer_control` being wired (real backend
    // only on macOS today), so registering it unconditionally here is safe:
    // it simply advertises as disabled wherever no backend exists. Real
    // sessions supply a `TuiBridgeResolver` so `request_access` surfaces the
    // approval dialog; `None` (offline/mobile) keeps the fail-closed
    // `DenyAllResolver` default.
    match computer_access_resolver {
        Some(resolver) => {
            tool_computer_use::register_all_with_access_resolver(reg, ctx.clone(), resolver);
        }
        None => tool_computer_use::register_all(reg, ctx.clone()),
    }
    // The two audio tools (`voice` + `speech`) from `tool-mobile`. Registered
    // ONLY where the capability behind them exists — i.e. where the host filled
    // `DesktopConfig::audio`, which today means a `bridge-server` connection
    // whose `AudioBridge` proxies to the Electron client. Two consequences worth
    // being explicit about:
    //
    // * With no audio wired (CLI, TUI, every offline factory) this call
    //   registers NOTHING, so the desktop tool list — and its locked snapshot —
    //   is byte-identical to what it was before audio existed.
    // * Only these two. `tool_mobile::register_all` would also add camera /
    //   notification / clipboard / share, device capabilities the desktop does
    //   not have; advertising a tool that can only ever fail is worse than not
    //   having it, since the model spends a turn learning something false.
    //
    // NOTE the gate is on the capability being INJECTED, not on the client
    // having proven it can serve it: the desktop cannot know in advance whether
    // the connected renderer implements audio. That is handled honestly at call
    // time instead — `AudioBridge` fails with "no desktop client is connected"
    // when nobody answers — and deliberately NOT with a protocol capability
    // handshake, which would be a negotiation invented for a client that may
    // simply be an older build.
    tool_mobile::register_audio(reg, &ctx);
    // `RemoteTrigger` gets the credential-store auth provider on desktop so it
    // can drive the claude.ai CCR API in-process. `register_all_with_auth`
    // registers `ScheduleCron` + `RemoteTrigger` (the latter with `cron_auth`)
    // and `ScheduleWakeup`; it returns the wakeup cell threaded out to `build`
    // → `DesktopRuntime` so the bridge fills it once the per-connection queue +
    // spawner exist (see `boot::assemble`).
    let (wakeup_cell, loop_wakeup_armed) =
        tool_cron::register_all_with_auth(reg, ctx.clone(), cron_auth);
    // In coordinator mode the richer `coordinator` `SendMessage` (registered
    // below, IN PLACE OF this builtin) carries the swarm routing surface, so we
    // skip the leaner `tool_ui` `SendMessage` here — otherwise, because the
    // registry's `find_by_name` is builtin-first, the earlier `tool_ui` copy
    // would silently shadow the coordinator one.
    if coordinator.is_some() {
        if let Some(resolver) = ask_user_question_resolver.clone() {
            tool_ui::register_all_except_send_message_with_ask_resolver(reg, ctx.clone(), resolver);
        } else if !advertise_ask_user_question {
            tool_ui::register_all_except_send_message_without_ask_user_question(reg, ctx.clone());
        } else {
            tool_ui::register_all_except_send_message(reg, ctx.clone());
        }
    } else if let Some(resolver) = ask_user_question_resolver {
        tool_ui::register_all_with_ask_resolver(reg, ctx.clone(), resolver);
    } else if !advertise_ask_user_question {
        tool_ui::register_all_without_ask_user_question(reg, ctx.clone());
    } else {
        tool_ui::register_all(reg, ctx.clone());
    }
    // PARITY (2.1.207 H-BIN-03): the `Artifact` tool (binary `eIs`, name `dw`).
    // Registered always on desktop; its `is_enabled` replicates CC's `dY()` gate
    // — the Statsig gate `tengu_cobalt_plinth` (code-default FALSE with no flag
    // backend) AND `allow_cobalt_plinth` AND first-party auth AND subscription
    // tier — so with no Statsig backend the tool registers DISABLED (invisible
    // to the model), byte-identical to the shipped binary on a host without the
    // `cobalt_plinth` gate. The publish/list claude.ai pipeline + the
    // `artifact-design`/`artifact-capabilities` bundled skills are Stage-2.
    reg.register_builtin(Arc::new(tool_ui::ArtifactTool::new(ctx.clone())));
    // SKILLEXEC.2: when a `SkillLoader` is supplied (real sessions wire the
    // `CommandRegistry`-backed loader), register the `Skill` tool with it so a
    // model-invoked skill resolves to a real slash command and expands. The
    // `None` path (offline registry-snapshot tests) keeps the hermetic
    // `EmptySkillLoader` — the registered tool NAME ("Skill") is identical
    // either way, so the locked tool-list snapshot is unaffected.
    match skill_loader {
        Some(loader) => {
            reg.register_builtin(Arc::new(tool_skill::SkillTool::with_loader(
                ctx.clone(),
                loader,
            )));
        }
        None => tool_skill::register_all(reg, ctx.clone()),
    }
    tool_task::register_all(reg, ctx.clone());
    // ----- desktop-only tool crates ----------------------------------------
    // Fusion is injected here (not inside `tool_agent::register_all`) so mobile
    // and snapshot tests keep an inert Agent tool.
    tool_agent::register_with_fusion_and_recorder_factory(
        reg,
        ctx.clone(),
        fusion,
        fusion_recorder.clone(),
        fusion_recorder_factory,
    );
    if let Some(CoordinatorWiring { team, spawn_seam }) = coordinator {
        for tool in coordinator::internal_tools::coordinator_internal_tools(
            team,
            spawn_seam,
            tool_ui::send_message::truncate_preview,
        ) {
            reg.register_builtin(tool);
        }
    }
    // (parity 2.1.212) Thread the transcript persister into EnterWorktree /
    // ExitWorktree so a create/enter writes a `worktree-state` entry and an exit
    // writes the clear record — the persist half of resume restoration. `None`
    // (offline factory / `--no-session-persistence`) leaves the tools on their
    // pre-persist path.
    tool_worktree::register_all_with_persister(reg, ctx.clone(), worktree_state_persister);
    tool_mcp::register_all(reg, ctx.clone());
    tool_lsp::register_all_with_live_cwd(reg, ctx, live_cwd);
    (wakeup_cell, loop_wakeup_armed)
}

/// Assemble the desktop builtin **skill** registry.
///
/// Delegates to `skill_api::register_desktop`, the single place that names
/// the desktop builtin skill set. Empty in M8 (no Rust-bundled skills yet —
/// skills are markdown loaded from disk by the session loader); the mobile
/// composition root will call `skill_api::register_mobile` instead.
#[must_use]
pub fn desktop_skill_registry() -> SkillRegistry {
    let mut reg = SkillRegistry::new();
    skill_api::register_desktop(&mut reg);
    reg
}

/// The single device-audio service a desktop host can inject.
///
/// `None` when the host has no audio path; the bridge path fills it from the
/// connection it is assembling. That service may report unknown support until
/// the connected client sends its initial capability snapshot.
#[derive(Clone)]
pub struct DesktopAudio {
    /// App-scoped device audio service.
    pub service: Arc<dyn lingxi_core::host::audio::AudioService>,
}

impl DesktopAudio {
    /// Build from one app-scoped audio service.
    #[must_use]
    pub fn from_single<T>(implementation: Arc<T>) -> Self
    where
        T: lingxi_core::host::audio::AudioService + 'static,
    {
        Self {
            service: implementation,
        }
    }
}

impl std::fmt::Debug for DesktopAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DesktopAudio(<configured>)")
    }
}

fn resolve_workflow_size_guideline(
    cfg: &DesktopConfig,
    cwd: &std::path::Path,
    managed_layers: &[lingxi_core::settings::SettingsJson],
) -> (tool_workflow::WorkflowSizeGuideline, bool, bool) {
    let defaults = lingxi_core::settings::SettingsJson {
        workflow_size_guideline: Some("medium".to_string()),
        ..Default::default()
    };
    let user_settings_path = cfg.lingxi_home.join("settings.json");
    let effective = lingxi_core::settings::Settings::load_with_layers_from_user_path(
        lingxi_core::settings::LoadInputs {
            env: &std::collections::BTreeMap::new(),
            project_dir: cwd,
            defaults,
        },
        lingxi_core::settings::FileLayerScope {
            include_user: cfg.setting_source_scope.0,
            include_project: cfg.setting_source_scope.1,
            include_local: cfg.setting_source_scope.1,
        },
        lingxi_core::settings::SupplementalLayers {
            cli_layer: cfg.flag_settings.as_ref(),
            managed_layers,
        },
        Some(&user_settings_path),
    )
    .ok();
    let wire = effective
        .as_ref()
        .and_then(|settings| settings.settings.workflow_size_guideline.as_deref())
        .unwrap_or("medium");
    let guideline = tool_workflow::WorkflowSizeGuideline::from_wire(wire);
    let source = effective
        .as_ref()
        .and_then(|settings| settings.effective_for("workflowSizeGuideline"))
        .and_then(|provenance| provenance.contributors.last())
        .copied();
    let managed = source == Some(lingxi_core::settings::tracer::Source::Managed);
    let is_default = source == Some(lingxi_core::settings::tracer::Source::Defaults);
    (guideline, managed, is_default)
}

fn resolve_workflow_session_enabled(
    cfg: &DesktopConfig,
    cwd: &std::path::Path,
    managed_layers: &[lingxi_core::settings::SettingsJson],
) -> (bool, bool) {
    let defaults = lingxi_core::settings::SettingsJson {
        enable_workflows: Some(true),
        ..Default::default()
    };
    let user_settings_path = cfg.lingxi_home.join("settings.json");
    let effective = lingxi_core::settings::Settings::load_with_layers_from_user_path(
        lingxi_core::settings::LoadInputs {
            env: &std::collections::BTreeMap::new(),
            project_dir: cwd,
            defaults,
        },
        lingxi_core::settings::FileLayerScope {
            include_user: cfg.setting_source_scope.0,
            include_project: cfg.setting_source_scope.1,
            include_local: cfg.setting_source_scope.1,
        },
        lingxi_core::settings::SupplementalLayers {
            cli_layer: cfg.flag_settings.as_ref(),
            managed_layers,
        },
        Some(&user_settings_path),
    )
    .ok();
    let enabled = effective
        .as_ref()
        .and_then(|settings| settings.settings.enable_workflows)
        .unwrap_or(true);
    let managed = effective
        .as_ref()
        .and_then(|settings| settings.effective_for("enableWorkflows"))
        .and_then(|provenance| provenance.contributors.last())
        .copied()
        == Some(lingxi_core::settings::tracer::Source::Managed);
    (enabled, managed)
}

/// Assemble the desktop slash-command registry.
///
/// Mirrors the boot sequence the CLI used inline before P6:
/// [`register_all_builtin_commands`] seeds the builtin handlers, then
/// [`register_core_batch_1`] + [`register_core_batch_2`] overwrite the wired
/// core handlers with their orchestrator/auth-bound implementations.
#[must_use]
pub async fn desktop_command_registry(
    handle: Arc<dyn OrchestratorHandle>,
    auth: Arc<dyn AuthHandle>,
    cwd: &std::path::Path,
    lingxi_home: &std::path::Path,
    connect_writer: Arc<dyn command_api::builtins::ConnectCredentialWriter>,
    connect_copilot: Arc<dyn command_api::builtins::CopilotConnectDriver>,
    connect_chatgpt: Arc<dyn command_api::builtins::ChatGptConnectDriver>,
    gates: CustomizationGates,
    strict_plugin_only_skills: bool,
    // `--add-dir` roots. Each contributes `<root>/<DOT_DIR>/skills` to skill
    // discovery, mirroring upstream's
    // `for(let e of Up()){ let S = P.join(e,".claude","skills"); … }`
    // (2.1.267 `src_172414592.js` @5180). Raw roots, NOT skill dirs: the join
    // happens below so all three registration sites share one answer.
    add_dir_roots: &[std::path::PathBuf],
    // SKILLEXEC: the SAME shared command-registry slot the slash dispatcher and
    // `Skill` tool loader observe (filled by `build()` right after this returns).
    // `/reload-skills` (batch 8) mutates it live so a reload refreshes the set
    // the rest of the session sees.
    shared_registry: Arc<RwLock<CommandRegistry>>,
) -> CommandRegistry {
    let mut reg = CommandRegistry::new();
    register_all_builtin_commands(&mut reg);
    // Bundled programmatic skills (`/loop`), port of `registerBundledSkills`.
    // Gated on the same cron kill-switch the scheduler uses
    // (`isKairosCronEnabled` ↔ `cron_scheduler_enabled(CLAUDE_CODE_DISABLE_CRON)`,
    // loop.ts:83). Registered AFTER builtins; `/loop` is not a builtin name so no
    // shadow conflict.
    let cron_enabled =
        cron_scheduler_enabled(std::env::var("CLAUDE_CODE_DISABLE_CRON").ok().as_deref());
    command_api::builtins::register_bundled_skills(&mut reg, cron_enabled);
    register_core_batch_1(&mut reg, handle.clone());
    register_core_batch_2(&mut reg, handle.clone(), auth.clone());
    // (H-BIN-09) Override the generic `/login` handler with one that enforces the
    // managed `forceLoginOrgUUID` org pin — the SAME pin `/connect` enforces via
    // `EngineOAuthConnect`. `register_builtin_handler` overwrites in place, so this
    // wins over the plain handler `register_core_batch_2` just registered. Hosts
    // without a managed policy tier (mobile) keep the plain, unrestricted handler.
    reg.register_builtin_handler(Arc::new(
        command_api::builtins::LoginHandler::new(auth)
            .with_org_policy(Arc::new(crate::desktop::connect::DesktopLoginOrgPolicy)),
    ));
    register_core_batch_4(&mut reg, handle.clone());
    register_core_batch_5(&mut reg, handle.clone());
    // Plan 3c: wire `/connect` over the engine-supplied credential-writer +
    // Copilot device-flow + ChatGPT OAuth seams.
    command_api::builtins::register::register_core_connect(
        &mut reg,
        connect_writer,
        connect_copilot,
        connect_chatgpt,
    );
    // Desktop-only command handlers: currently none — the desktop command names
    // (/commit, /diff, /review, /chrome, /ide, …) are served as command-core
    // unimplemented stubs. Register real desktop handlers on `reg` directly here
    // when a future milestone implements them.
    // SLASH.2: discover + register custom `.lingxi/commands/**.md` commands
    // (project up to git-root/home, plus user + managed layers), the same
    // layering claude-code's getCommands uses. Registered AFTER builtins so a
    // same-named custom command shadows a builtin (TS findCommand order).
    let home = dirs::home_dir().unwrap_or_else(|| lingxi_home.to_path_buf());
    let managed_dir = crate::desktop::settings_watch::managed_settings_dir();
    // The `--add-dir` skill tier. `load_skill_markdown_files_with_roots` takes
    // ALREADY-RESOLVED skill directories (a test pins that contract), so the
    // `<root>/<DOT_DIR>/skills` join belongs to the caller — here.
    let additional_skill_dirs: Vec<std::path::PathBuf> = add_dir_roots
        .iter()
        .map(|root| root.join(branding::DOT_DIR).join("skills"))
        .collect();
    // Batch 8: the newly-ported implemented commands (`/fork`, `/goal`,
    // `/recap`, `/reload-skills`, `/skill-doctor`, `/stop`). Wired here (after
    // the skill-discovery roots are known, before the `disables_skills` early
    // return) so the builtins register regardless of the customization gate.
    command_api::builtins::register_core_batch_8(
        &mut reg,
        handle.clone(),
        shared_registry,
        cwd.to_path_buf(),
        lingxi_home.to_path_buf(),
        Some(managed_dir.clone()),
        home.clone(),
        additional_skill_dirs.clone(),
        gates.safe_mode,
        load_merged_disable_agent_view(cwd),
    );
    // (M3 cc2.1.198) `--safe-mode` / `--bare` disable custom-command + skill
    // dir discovery (`K5d.skills:!1` / `V5d.skills:!0`; the commands-dir
    // loader `cWa` bails on `xd()||Hc("skills")`). Builtins above stay — only
    // the on-disk customization layers are skipped, including the managed dir
    // (the binary's `aGe` returns `[]` before reaching its managed root).
    if gates.disables_skills() {
        return reg;
    }
    let (registered, registered_skills) = if strict_plugin_only_skills {
        (
            command_api::builtins::load_and_register_managed_custom_commands(
                &mut reg,
                &managed_dir,
            )
            .await,
            command_api::builtins::load_and_register_managed_skill_commands(&mut reg, &managed_dir)
                .await,
        )
    } else {
        (
            command_api::builtins::load_and_register_custom_commands(
                &mut reg,
                cwd,
                lingxi_home,
                &managed_dir,
                &home,
            )
            .await,
            command_api::builtins::load_and_register_skill_commands_with_roots(
                &mut reg,
                cwd,
                lingxi_home,
                Some(&managed_dir),
                &home,
                &additional_skill_dirs,
            )
            .await,
        )
    };
    reg.register_builtin_handler(Arc::new(
        command_api::builtins::SkillsHandler::with_all_roots(
            cwd.to_path_buf(),
            lingxi_home.to_path_buf(),
            Some(managed_dir),
            additional_skill_dirs,
        ),
    ));
    tracing::debug!(
        custom_commands = registered,
        skill_commands = registered_skills,
        "registered custom slash commands"
    );
    reg
}

/// Everything a host needs to drive a conversation, built deterministically by
/// [`build`] from a [`DesktopConfig`].
///
/// This is the lifted shape of `apps/cli`'s `Runtime` (F2-01): moving the
/// runtime wiring out of the CLI binary lets the bridge-server (which CANNOT
/// depend on `apps/cli` — app→app is a leaf, `scripts/checks/check_deps.py:99`)
/// construct an identical orchestrator. The CLI now derives a `DesktopConfig`
/// from `Argv`/env and calls `build`.
/// (`!` bash mode) Desktop implementation of the TUI's
/// [`tool_api::bash_runner::BashRunner`] seam.
///
/// Runs a TUI `!command` through the SAME sandboxed [`tool_shell::BashTool`] the
/// model's `Bash` tool uses — NEVER a raw `std::process`/`Command`. It holds a
/// clone of the session [`BuiltinToolContext`] (which carries the live
/// `sandbox_runner` + `sandbox_runtime` config + process runner), constructs a
/// fresh `BashTool` per call, and maps the tool's result `data.{stdout,stderr}`
/// into a [`tool_api::bash_runner::BashRunOutput`]. Because the command rides the same
/// `BashTool::call` path, it is wrapped by the same M2-04 sandbox decision matrix
/// and `sandbox-runtime` runner as a model-issued Bash call. `BashTool`'s
/// `check_permissions` is an allow-all gate, so a user-typed `!` runs sandboxed
/// without a separate permission prompt (matching claude-code's bash mode).
struct DesktopBashRunner {
    ctx: BuiltinToolContext,
}

#[async_trait::async_trait]
impl tool_api::bash_runner::BashRunner for DesktopBashRunner {
    async fn run(&self, command: &str) -> tool_api::bash_runner::BashRunOutput {
        use tool_api::Tool as _;
        let tool = tool_shell::BashTool::new(self.ctx.clone());
        // Progress channel is required by the `Tool::call` signature but Bash
        // emits no progress for a foreground run; drop the receiver.
        let (progress_tx, _progress_rx) = tool_api::progress_channel();
        // A minimal per-call context for a user-initiated `!` command: no
        // tool_use_id, empty history, inert options. The model id is unused for
        // execution (only `BashTool::prompt` reads it).
        let use_ctx = tool_api::ToolUseContext::model_seed(self.ctx.default_model.clone());
        match tool
            .call(
                serde_json::json!({ "command": command }),
                use_ctx,
                progress_tx,
            )
            .await
        {
            Ok(result) => {
                let field = |key: &str| {
                    result
                        .data
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                tool_api::bash_runner::BashRunOutput {
                    stdout: field("stdout"),
                    stderr: field("stderr"),
                    exit_code: result
                        .data
                        .get("exit_code")
                        .and_then(serde_json::Value::as_i64)
                        .and_then(|c| i32::try_from(c).ok())
                        .unwrap_or_default(),
                }
            }
            // A spawn/IO/validation error surfaces as stderr text so the TUI
            // still renders a `UserBashOutput` row (no LLM turn, no raw spawn).
            // A spawn/validation failure never ran a command, so there is no
            // status to report; 1 is the conventional "did not succeed".
            Err(e) => tool_api::bash_runner::BashRunOutput {
                stdout: String::new(),
                stderr: e.to_string(),
                exit_code: 1,
            },
        }
    }
}

/// Project-specific `/worktree` slash-command grammar. Claude Code exposes the
/// same lifecycle through `--worktree` plus `EnterWorktree`/`ExitWorktree`, but
/// has no slash row; `LingXi` keeps the upstream command table locked and adds
/// this desktop-only convenience handler at composition time instead.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorktreeSlashAction {
    Create(Option<String>),
    Enter(String),
    Status,
    Keep,
    Remove { discard_changes: bool },
}

const WORKTREE_SLASH_USAGE: &str =
    "Usage: /worktree [status|create [name]|enter <path>|keep|remove [--discard]]";

fn parse_worktree_slash_action(
    args: &ParsedSlashCommand,
) -> Result<WorktreeSlashAction, &'static str> {
    let tokens = &args.positional_args;
    if tokens.is_empty() {
        return Ok(WorktreeSlashAction::Create(None));
    }

    match tokens[0].as_str() {
        "status" if tokens.len() == 1 => Ok(WorktreeSlashAction::Status),
        "create" if tokens.len() == 1 => Ok(WorktreeSlashAction::Create(None)),
        "create" if tokens.len() == 2 => Ok(WorktreeSlashAction::Create(Some(tokens[1].clone()))),
        // Joining the remaining quote-aware tokens accepts both
        // `enter "/path with spaces"` and the forgiving unquoted form.
        "enter" if tokens.len() >= 2 => Ok(WorktreeSlashAction::Enter(tokens[1..].join(" "))),
        // Do not let a missing path fall through to the `<name>` shorthand and
        // accidentally create a worktree literally named `enter`.
        "enter" => Err(WORKTREE_SLASH_USAGE),
        "keep" if tokens.len() == 1 => Ok(WorktreeSlashAction::Keep),
        "remove" if tokens.len() == 1 => Ok(WorktreeSlashAction::Remove {
            discard_changes: false,
        }),
        "remove" if tokens.len() == 2 && tokens[1] == "--discard" => {
            Ok(WorktreeSlashAction::Remove {
                discard_changes: true,
            })
        }
        // `/worktree <name>` mirrors the CLI's `--worktree <name>` shorthand.
        _ if tokens.len() == 1 => Ok(WorktreeSlashAction::Create(Some(tokens[0].clone()))),
        _ => Err(WORKTREE_SLASH_USAGE),
    }
}

/// Desktop slash handler backed by the exact same tools as model-issued
/// worktree operations. This keeps validation, cwd swaps, dirty-worktree
/// protection, telemetry, hooks, and transcript state persistence on one path.
struct DesktopWorktreeCommandHandler {
    ctx: BuiltinToolContext,
    state_persister: Option<Arc<dyn tool_api::WorktreeStatePersister>>,
}

impl DesktopWorktreeCommandHandler {
    fn new(
        ctx: BuiltinToolContext,
        state_persister: Option<Arc<dyn tool_api::WorktreeStatePersister>>,
    ) -> Self {
        Self {
            ctx,
            state_persister,
        }
    }

    fn status(&self) -> String {
        let session = self
            .ctx
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(session) = session else {
            return "No active worktree session.".to_string();
        };

        let branch = if session.branch_name.is_empty() || session.branch_name == "HEAD" {
            "(detached HEAD)"
        } else {
            &session.branch_name
        };
        let ownership = if session.entered_existing {
            "entered existing (keep only)"
        } else {
            "created by this session"
        };
        let mut lines = vec![
            format!("Worktree: {}", session.worktree_path.display()),
            format!("Branch: {branch}"),
            format!("Original directory: {}", session.original_cwd.display()),
            format!("Ownership: {ownership}"),
        ];
        if let Some(tmux) = session.tmux_session_name {
            lines.push(format!("Tmux session: {tmux}"));
        }
        lines.join("\n")
    }

    fn enter_tool(&self) -> tool_worktree::EnterWorktreeTool {
        let tool = tool_worktree::EnterWorktreeTool::new(self.ctx.clone());
        match &self.state_persister {
            Some(persister) => tool.with_state_persister(persister.clone()),
            None => tool,
        }
    }

    fn exit_tool(&self) -> tool_worktree::ExitWorktreeTool {
        let tool = tool_worktree::ExitWorktreeTool::new(self.ctx.clone());
        match &self.state_persister {
            Some(persister) => tool.with_state_persister(persister.clone()),
            None => tool,
        }
    }

    async fn call_tool<T: tool_api::Tool + Sync>(
        &self,
        tool: &T,
        input: serde_json::Value,
    ) -> String {
        let (progress_tx, _progress_rx) = tool_api::progress_channel();
        let use_ctx = tool_api::ToolUseContext::model_seed(self.ctx.default_model.clone());
        match tool.call(input, use_ctx, progress_tx).await {
            Ok(result) => result
                .model_content
                .or_else(|| {
                    result
                        .data
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| result.data.to_string()),
            Err(error) => error.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl BuiltinCommandHandler for DesktopWorktreeCommandHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        use WorktreeSlashAction::{Create, Enter, Keep, Remove, Status};

        let display = match parse_worktree_slash_action(args) {
            Ok(Create(name)) => {
                let input = name.map_or_else(
                    || serde_json::json!({}),
                    |name| serde_json::json!({ "name": name }),
                );
                self.call_tool(&self.enter_tool(), input).await
            }
            Ok(Enter(path)) => {
                self.call_tool(&self.enter_tool(), serde_json::json!({ "path": path }))
                    .await
            }
            Ok(Status) => self.status(),
            Ok(Keep) => {
                self.call_tool(&self.exit_tool(), serde_json::json!({ "action": "keep" }))
                    .await
            }
            Ok(Remove { discard_changes }) => {
                self.call_tool(
                    &self.exit_tool(),
                    serde_json::json!({
                        "action": "remove",
                        "discard_changes": discard_changes,
                    }),
                )
                .await
            }
            Err(usage) => usage.to_string(),
        };
        CommandResult::Done {
            display: Some(display),
        }
    }

    fn name(&self) -> &str {
        "worktree"
    }

    fn description(&self) -> &str {
        "Create, enter, inspect, or exit a worktree"
    }

    fn allowed_tools(&self) -> &'static [&'static str] {
        &[
            tool_worktree::worktree::ENTER_TOOL_NAME,
            tool_worktree::worktree::EXIT_TOOL_NAME,
        ]
    }
}

pub struct DesktopRuntime {
    /// Catalog notifications owned by this credential scope.
    pub catalog_registry: FusionCatalogRegistry,
    /// Product region actually selected by the running client.
    pub provider_region: llm_runtime::Region,
    /// The fully-constructed orchestrator (cost tracker + MCP/hook/agent
    /// registries + compaction wired), bound to the supplied output stream and
    /// permission gate.
    pub orchestrator: Arc<ConversationOrchestrator>,
    /// Hydrated per-session durable coordinator retained for Fusion terminal
    /// receipts and ordinary cost mutations. Every host has one: an
    /// ephemeral-transcript host gets a disposable ledger under a temporary
    /// home rather than no ledger at all.
    pub session_state: Arc<session_state::SessionStateCoordinator>,
    /// Common Fusion recorder pinned to the boot session's coordinator.
    pub fusion_recorder: Arc<dyn lingxi_core::host::FusionRunRecorder>,
    /// Per-session Fusion recorder factory retained for host shutdown/remount
    /// draining. It owns recorders for every mounted session, not just boot A.
    pub fusion_recorder_factory: Arc<fusion_recorder::DesktopFusionRecorderFactory>,
    /// Shared ordered shutdown owner used by CLI remounts and bridge process
    /// teardown. It is always present, including explicit ephemeral mode.
    pub session_lifecycle: Arc<DesktopSessionLifecycle>,
    /// The runtime analytics bus shared with the orchestrator and host-side
    /// producers. `mcp serve` uses this to emit process-scope startup
    /// telemetry after boot succeeds and before the request loop starts.
    pub analytics_bus: Arc<telemetry::AnalyticsBus>,
    /// Shared slash-command registry populated during build and observed by both
    /// the dispatcher and skill/plugin loaders. Surfaced so non-TUI hosts can
    /// snapshot the live catalog and detect command-set mutations.
    pub shared_command_registry: Arc<RwLock<CommandRegistry>>,
    /// Slash-command dispatcher seeded with the builtin handlers + wired core
    /// handlers (the `register_all_builtin_commands` → `register_core_batch_1`
    /// → `register_core_batch_2` sequence).
    pub dispatcher: RegistrySlashDispatcher,
    /// Auth handle for `/login` and `/logout`.
    pub auth: Arc<dyn AuthHandle>,
    /// The desktop task registry shared with the tool context (the TUI / a
    /// transport wraps it in a poller to read live background-task state).
    pub task_registry: Arc<tasks::registry::TaskRegistry>,
    /// Test-only handle to the exact Workflow tool registered by the desktop
    /// composition root. Keeping this observable lets the composition test
    /// exercise the live permission gate rather than a separately-built tool.
    #[cfg(test)]
    pub(crate) wired_workflow_tool: Arc<tool_workflow::WorkflowTool>,
    /// Event-driven lifecycle/progress feed for the interactive TUI. Hosts
    /// take this receiver once and merge it into their existing TurnEvent
    /// channel; a host that does not render a TUI may simply drop it.
    pub workflow_events: Option<tokio::sync::mpsc::UnboundedReceiver<DesktopWorkflowEvent>>,
    /// M10: the per-session coordinator team registry. One is constructed per
    /// `build()` regardless of mode so the status feed (and the PHASE-2 command
    /// router) always have a handle to read; it is observable but empty (no
    /// workers) until Agent launches teammates.
    pub coordinator: Arc<coordinator::TeamRegistry>,
    /// M10: the per-session coordinator-mode flag. Entered at build time only
    /// when `cfg.session_started_as_coordinator` is `true`; otherwise this is
    /// constructed disabled (`is_enabled() == false`) and a default session is
    /// byte-identical to the pre-M10 build. A `call()`-time gate also consults
    /// it (defense-in-depth) so a future `/coordinator exit()` can neutralize
    /// the tools without a registry rebuild.
    pub coordinator_mode: Arc<coordinator::CoordinatorMode>,
    /// The connection-scoped [`AdapterPermissionGate`] handle, present ONLY when
    /// `cfg.use_noop_permission_gate` is `false`. The transport calls
    /// [`AdapterPermissionGate::resolve`] on this to satisfy a parked `check()`
    /// from an inbound `ApprovePermission`/`DenyPermission` (F2-06). `None` when
    /// the host opted into the always-allow `NoOpPermissionGate` (the CLI).
    pub permission_gate: Option<Arc<AdapterPermissionGate>>,
    /// The ENFORCING permission gate (the `PolicyPermissionGate` wrapping the
    /// injected prompt transport, or the base gate when enforcement is off). The
    /// interactive TUI holds this to drive Shift+Tab live permission-mode
    /// cycling via [`permission::gate::PermissionGate::set_permission_mode`], so
    /// enforcement follows the bottom-of-composer mode indicator. `None` only
    /// when no gate was built.
    pub enforcing_permission_gate: Option<Arc<dyn PermissionGate>>,
    /// Live settings watcher firing `ConfigChange` hooks when the user /
    /// project / local / policy settings files mutate on disk (parity:
    /// claude-code `changeDetector.ts` → `executeConfigChangeHooks`). Held by
    /// the runtime so it lives for the session; dropping the runtime aborts the
    /// watch tasks (RAII teardown). `None`-shaped as an empty handle (no tasks)
    /// when no `.claude` directory exists to watch.
    pub settings_watcher: settings_watch::SettingsWatcherHandle,
    /// Live file-changed watcher firing `FileChanged` hooks when a path resolved
    /// from a `FileChanged` hook's `matcher` mutates on disk (parity: claude-code
    /// `fileChangedWatcher.ts` → `executeFileChangedHooks`). Held by the runtime
    /// so it lives for the session; dropping the runtime aborts the watch tasks
    /// (RAII teardown). An empty handle (no tasks) when no `FileChanged` hook is
    /// configured — the no-watch case is byte-identical to before.
    pub file_changed_watcher: file_changed_watch::FileChangedWatcherHandle,
    /// Live hook registry shared by the orchestrator and plugin manager.
    /// Bridge-server uses this handle for source-scoped atomic hot reloads.
    pub hook_registry: Arc<RwLock<hooks::HookRegistry>>,
    /// Live skill/plugin catalog refresher shared with runtime root reloads.
    pub repo_root_reloader: Arc<dyn lingxi_core::host::RepoRootReloader>,
    /// Shared Claude.ai subscription snapshot (Task 4). Seeded at build time
    /// with the scope-derived `is_subscriber` flag; for subscribers a
    /// background OAuth profile + roles fetch overwrites it with the full
    /// tier/billing/role snapshot once the endpoints respond. UI layers read
    /// it at compose time and treat `None` / a poisoned lock as the
    /// conservative default snapshot.
    pub subscription: lingxi_core::host::subscription::SharedSubscription,
    /// (`/sandbox`) The shared fast-toggle cell for bash-command sandboxing.
    /// The SAME `Arc<AtomicBool>` the bash tool reads via
    /// `BuiltinToolContext::sandbox_enabled_override`; the TUI mount threads a
    /// clone into the widget so `/sandbox` flips it for the live session.
    pub sandbox_toggle: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// (`/sandbox` description) `SandboxRuntimeConfig.autoAllowBashIfSandboxed`
    /// — renders " (auto-allow)" in the dynamic `/sandbox` popup description.
    pub sandbox_desc_auto_allow: bool,
    /// (`/sandbox` description) `SandboxRuntimeConfig` unsandboxed-commands-allowed
    /// — renders ", fallback allowed" in the dynamic `/sandbox` description.
    pub sandbox_desc_fallback: bool,
    /// (`/sandbox` description) `checkDependencies().errors.length === 0` — when
    /// `false`, the dynamic `/sandbox` description shows the warning glyph.
    pub sandbox_desc_deps_ok: bool,
    /// (`/rewind`) The shared file-history checkpoint store. The SAME
    /// `Arc<session::FileHistory>` the orchestrator captures into; the CLI uses
    /// it to build the `/rewind` picker rows and to restore code on rewind.
    pub file_history: std::sync::Arc<session::FileHistory>,
    /// (`/reload-plugins`) The retained plugin subsystem, or `None` when plugins
    /// are disabled for the session. The CLI threads it into the TUI mount so the
    /// interactive `/reload-plugins` command applies pending enable/disable
    /// changes to the live session (see [`PluginRuntime::refresh`]).
    pub plugin_runtime: Option<std::sync::Arc<PluginRuntime>>,
    /// Phase 2a §6.2: per-`profile_name` availability flag driving the `/model`
    /// picker's Connect badge (a sibling map, NOT a field on the frozen
    /// `ModelListing`). The tui joins it by provider/profile name.
    pub provider_availability: std::collections::BTreeMap<String, bool>,
    /// Set when the boot-time connected-provider fallback rerouted the session
    /// default model (its configured provider was definitively disconnected).
    /// Hosts surface it: the CLI prints a stderr notice pre-alt-screen; the
    /// bridge relies on the `tracing::warn!` `build()` already emitted. `None`
    /// ⟶ the configured default booted unchanged.
    pub default_model_fallback: Option<DefaultModelFallbackNotice>,
    /// Provenance of the model that the engine selected for the session's
    /// initial/default row. Provider-neutral so managed policy is not inferred
    /// from an Anthropic-specific auth or profile name.
    pub model_provenance: lingxi_core::host::ModelProvenance,
    /// (T2a) Per-provider login method tag, keyed by profile_name, derived from
    /// the real catalog auth strategy: "api_key" | "copilot_device" | "oauth".
    /// Threaded into the TUI so the /connect picker shows the real method.
    pub provider_auth_methods: std::collections::BTreeMap<String, String>,
    /// Phase 2a I1/I2: authoritative `request_model -> (profile_name,
    /// provider_label)` map assembled from the LIVE multi-provider
    /// `ClientConfig.providers` (every profile's `models[].request_model`). The
    /// tui joins it in the `/model` picker so a bare available-model id from a
    /// USER-defined provider resolves to its OWN provider group and gates on
    /// `provider_availability` — instead of mis-falling into `"builtin"`/`true`.
    /// Built-in CATALOG rows are unaffected (they group via the orchestrator's
    /// `list_model_listings`).
    pub model_providers: std::collections::BTreeMap<String, (String, String)>,
    /// Phase 2a: the concrete routing adapter, surfaced read-only so host/tests
    /// can inspect the wired fallback chains.
    pub provider_adapter: Arc<ProviderApiAdapter>,
    /// Phase 2a C1: the shared credential manager (keychain-backed). Surfaced so
    /// the host can thread it onto the TUI App, where the `/connect` screen
    /// persists a collected key (`CredentialManager::set_provider_key`). Same
    /// `Arc` the orchestrator already holds — no second store is constructed.
    pub credentials: Arc<secret::CredentialManager>,
    /// Shared HTTP transport for TUI-owned client-side WebSearch test runs.
    pub http: Arc<dyn lingxi_core::host::HttpTransport>,
    /// Structured-output capture slot — `Some` only when `--json-schema` is set
    /// (`DesktopConfig.json_schema`). The forced `StructuredOutput` tool writes
    /// the model's result here; the print path reads it after each turn to
    /// validate against the schema and retry. `None` for every normal run.
    pub structured_output_slot: Option<orchestrator::structured_output::StructuredOutputSlot>,
    /// `/loop` dynamic-mode (Phase 2): the set-once cell for the registered
    /// `ScheduleWakeup` tool. Empty at build time (the per-connection queue +
    /// spawner don't exist yet); the bridge composition root fills it at
    /// `boot::assemble` with a `MsgQueueWakeupScheduler`. Interactive CLI hosts
    /// retain and bind the same seam when mounting their local queue. One-shot
    /// hosts without a queue leave the cell empty.
    pub wakeup_scheduler_cell: tool_cron::WakeupSchedulerCell,
    /// A `RuntimeSpawner` for host-side background wiring that needs one after
    /// `build` (today: the bridge's `MsgQueueWakeupScheduler`, which sleeps then
    /// enqueues a `/loop` self-wakeup). A fresh stateless `PosixRuntime` — the
    /// same seam every in-`build` spawner uses (D17: never a direct
    /// `tokio::spawn`).
    pub runtime_spawner: Arc<dyn lingxi_core::host::RuntimeSpawner>,
    /// (`!` bash mode) The sandboxed Bash runner for the TUI's `!command` path,
    /// built over the SAME `BuiltinToolContext` (sandbox runner + runtime config)
    /// the model's `Bash` tool uses. The CLI threads it into the TUI `Runtime`
    /// (`Runtime::with_bash_runner`) so a typed `!ls` runs sandboxed and renders
    /// inline with no LLM turn — never a raw process.
    pub bash_runner: Arc<dyn tool_api::bash_runner::BashRunner>,
    /// (#3 shell-expansion) The shared prompt shell-expansion provider, built
    /// over the SAME `BuiltinToolContext` the dispatcher + Bash tool use. The CLI
    /// threads a clone into `apps/cli`'s `Runtime.shell_expansion` → the ratatui
    /// TUI's `ChatWidget`, so a typed `/commit` expands its embedded `!`git …``
    /// bodies through the real host runner + policy-backed gate before submit —
    /// the same expansion the dispatcher performs for non-TUI hosts.
    pub shell_expansion: Arc<dyn command_api::ShellExpansionProvider>,
    /// (`/connect` Copilot device-flow) The GitHub-Copilot OAuth device-flow
    /// driver (`EngineCopilotConnect` over `PosixHttp`). The CLI threads a clone
    /// into `tui::session::Runtime::with_copilot_connect_driver` so picking
    /// GitHub Copilot in `/connect` runs the real web sign-in (browser open +
    /// device-code poll + token store) instead of an inert key field. Also
    /// registered in the engine `/connect` command group (same Arc).
    pub connect_copilot: Arc<dyn command_api::builtins::CopilotConnectDriver>,
    /// (T2b) Unified OAuth sign-in driver for the TUI `/connect` picker. Drives
    /// the browser flow for the first-party OAuth providers (Anthropic Pro/Max,
    /// OpenAI ChatGPT) — replacing the honest-but-inert `Unavailable` screen.
    pub oauth_connect_driver: Arc<dyn command_api::builtins::OAuthConnectDriver>,
    /// (P1-08 runtime `/add-dir`) The SAME `Arc<SessionCwd>` the file tools gate
    /// on. The CLI `/add-dir` effect calls `add_trusted_dir(...)` on it so a
    /// directory added mid-session is immediately accessible to
    /// Read/Edit/Write/Glob/Grep/NotebookEdit without a reboot.
    pub session_cwd: Arc<SessionCwd>,
    /// (P1-08 runtime `/add-dir`) The live MCP registry. The CLI `/add-dir`
    /// effect calls `add_root(...)` + `notify_roots_list_changed_all()` on it so
    /// every connected server's `roots/list` reflects the new working directory.
    pub mcp_registry: Arc<mcp::McpRegistry>,
    /// Provider-neutral local IDE endpoint lifecycle. The handle owns secure
    /// lockfile discovery and local auth tokens; callers only see redacted
    /// status and action results.
    pub ide_handle: Arc<dyn lingxi_core::host::IdeHandle>,
    /// The assembled tool registry — the SAME `Arc` the orchestrator dispatches
    /// through.
    ///
    /// PRIVATE, and the `Arc` must not escape: `ToolRegistry`'s MCP partition
    /// sits behind an `RwLock`, so `register_mcp_tools` / `unregister_mcp_tools`
    /// take `&self`. Anyone holding a clone of this handle could drop a live
    /// connection's tools out of dispatch while `McpRegistry` still believes
    /// that connection is up — a mid-session tool-list flap racing the
    /// generation-checked catalog-refresh task, which is the ONE legitimate
    /// `&self` caller. [`DesktopRuntime::registered_tool_names`] answers the
    /// only question anyone outside has needed so far, and answers it by value.
    tools: Arc<ToolRegistry>,
    /// The device-audio capability this runtime was built with — the very
    /// `Arc`s placed in the tool context, not a second read of the config.
    /// `None` unless the host filled [`DesktopConfig::audio`].
    ///

    /// PRIVATE: nothing outside needs the trait objects (the tools hold their
    /// own clones through the context). It is kept because
    /// [`DesktopRuntime::has_audio`] must answer from the config→build path
    /// INDEPENDENTLY of the registry — deriving audio-presence from the
    /// registered tool names would make "the capability reached the tool
    /// context" unfalsifiable, since the tools are registered *because* of the
    /// capability.
    audio: Option<DesktopAudio>,
}

impl DesktopRuntime {
    /// The names of every tool this build registered, by value.
    ///

    /// The honest observation point for a capability-gated tool: the tool
    /// context itself is consumed by [`build`], so "did the capability reach
    /// the engine" can only be asked of what the registry ended up holding.
    /// Returns names rather than the registry handle — see the `tools` field
    /// for why that handle must not escape.
    #[must_use]
    pub fn registered_tool_names(&self) -> Vec<String> {
        self.tools.all_names()
    }

    /// Whether this runtime was built with a device-audio capability
    /// ([`DesktopConfig::audio`]).
    ///

    /// Read from the capability itself, not from the tool list, so the two
    /// together distinguish "the config never reached the runtime" from "it
    /// reached the runtime but not the tool context".
    #[must_use]
    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }
}

/// Push-only workflow updates emitted by the desktop composition root.
///
/// The registry remains authoritative for snapshots and control operations;
/// this feed only removes the old timer/polling dependency from the live TUI
/// path. It intentionally carries the task runtime's structured progress DTO
/// instead of depending on a presentation crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopWorkflowEvent {
    /// One structured phase/agent update from a running workflow.
    Progress {
        task_id: String,
        run_id: String,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    },
    /// A workflow task transitioned to a new registry status.
    Status {
        task_id: String,
        status: tasks::TaskStatus,
    },
}

struct DesktopWorkflowEventSink {
    registry: Arc<tasks::registry_status_sink::RegistryStatusSink>,
    tx: tokio::sync::mpsc::UnboundedSender<DesktopWorkflowEvent>,
}

#[async_trait::async_trait]
impl tasks::handlers::TaskStatusSink for DesktopWorkflowEventSink {
    async fn set_status(&self, task_id: &str, status: tasks::TaskStatus) {
        tasks::handlers::TaskStatusSink::set_status(&*self.registry, task_id, status).await;
        let _ = self.tx.send(DesktopWorkflowEvent::Status {
            task_id: task_id.to_owned(),
            status,
        });
    }

    async fn is_registered(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_registered(&*self.registry, task_id).await
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        tasks::handlers::TaskStatusSink::is_terminal(&*self.registry, task_id).await
    }
}

#[async_trait::async_trait]
impl tasks::handlers::local_workflow::WorkflowProgressSink for DesktopWorkflowEventSink {
    async fn emit_workflow_progress(
        &self,
        task_id: &str,
        run_id: &str,
        progress: tasks::handlers::local_workflow::WorkflowProgressUpdate,
    ) {
        let _ = self.tx.send(DesktopWorkflowEvent::Progress {
            task_id: task_id.to_owned(),
            run_id: run_id.to_owned(),
            progress,
        });
    }
}

/// Construct the live sandbox runner used by desktop composition roots.
///
/// Standalone CLI workflows such as `plugin eval --scaffold` use this factory
/// so they receive the same runtime-backed filesystem and network enforcement
/// as model-invoked shell tools without depending on the concrete runner implementation.
#[must_use]
pub fn new_live_sandbox_runner() -> Arc<dyn tool_api::SandboxRunner> {
    Arc::new(sandbox_runner::SandboxRuntimeRunner::new())
}

/// `XV` — the name upstream's `mUe` puts on the synthetic tool call it hands
/// the classifier for a sandboxed outbound connection.
///
/// It matters that this is the upstream spelling and not a local one: the
/// classifier renders whatever name it is given straight into its prompt, and
/// the bundled policy's rule — labelled `Sandbox Network Callback` — says in its
/// body "A `SandboxNetworkAccess` action". A local name would ask the
/// classifier to match a rule against a name the rule never mentions.
const SANDBOX_NETWORK_TOOL: &str = "SandboxNetworkAccess";

fn sandbox_network_ask_callback(permission_gate: Arc<dyn PermissionGate>) -> sandbox_runner::AskFn {
    // `ive` — upstream memoises the verdict per `host:port`. Its ALLOW arm is
    // keyed on a transcript watermark (`CLe`: message count + last uuid) and
    // expires when the conversation moves on; its BLOCK arm is `reuse:"always"`
    // and stands for the session; `unavailable` is never cached at all.
    //
    // This callback is handed only a host and a port, with no watermark to key
    // an allow on, so only the arm that needs none is ported: a blocked host
    // stays blocked without paying to ask again. Allows still pay every time,
    // which is what this build did before — the subset can only ever be more
    // conservative than upstream, never more permissive.
    let blocked: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    Arc::new(move |host, port| {
        let permission_gate = Arc::clone(&permission_gate);
        let blocked = Arc::clone(&blocked);
        let host = host.to_owned();
        Box::pin(async move {
            // `${e}:${n??"*"}` — this callback's port is not optional, so the
            // `*` arm has no counterpart.
            let key = format!("{host}:{port}");
            if blocked
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(&key)
            {
                return Ok(false);
            }
            let input = serde_json::json!({
                "host": host,
                "port": port,
            });
            let allow = matches!(
                permission_gate.check(SANDBOX_NETWORK_TOOL, &input).await,
                lingxi_core::host::PermissionDecision::Allow
            );
            if !allow {
                blocked
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(key);
            }
            Ok(allow)
        })
    })
}

fn new_live_sandbox_runner_with_permission_gate(
    permission_gate: Arc<dyn PermissionGate>,
) -> Arc<dyn tool_api::SandboxRunner> {
    Arc::new(sandbox_runner::SandboxRuntimeRunner::with_ask_callback(
        sandbox_network_ask_callback(permission_gate),
    ))
}

/// Errors surfaced while building a [`DesktopRuntime`].
///
/// Lifted from `apps/cli`'s `InitError`. Construction is effectively infallible
/// today (the orchestrator constructor cannot fail), but the typed error is kept
/// so future iterations (real OAuth bootstrap, MCP connect) can surface a cause
/// without changing every call site.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// API base URL resolution / api-client construction failed.
    #[error("api base resolution failed: {0}")]
    ApiBase(String),
    /// Custom beta headers were requested for a route/auth mode that cannot
    /// safely carry Anthropic first-party API-key beta headers.
    #[error("custom betas require a first-party Anthropic API-key session")]
    InvalidCustomBetas,
    /// Orchestrator construction failed.
    #[error("orchestrator construction failed: {0}")]
    Orchestrator(String),
    /// Durable session claim/hydration/composition failed before activation.
    #[error("durable session state failed: {0}")]
    DurableSession(String),
    /// Secure-storage backend initialization failed.
    #[error("secure storage init failed: {0}")]
    SecureStorage(String),
    /// `sandbox.enabled` and `sandbox.failIfUnavailable` are both set, but the
    /// sandbox cannot run on this host (unsupported platform / WSL1 / missing
    /// deps / platform not in `sandbox.enabledPlatforms`). Faithful to
    /// claude-code's `isSandboxRequired()` startup refusal (sandbox-adapter.ts:479)
    /// — refusing rather than silently ignoring the operator's security posture
    /// (issue #34044).
    #[error("sandbox required but unavailable: {0}")]
    SandboxUnavailable(String),
    /// `cfg.worktree_launch` was `Some` (the user passed `-w`/`--worktree`)
    /// but the requested worktree could not be created (invalid slug, no git
    /// repo, git failure, ...). A HARD boot failure — worktree-tmux-launch
    /// plan Task 3: the user explicitly asked for an isolated worktree, so a
    /// silent fall-through to the plain cwd would be a surprising, unrequested
    /// downgrade rather than a recoverable default.
    #[error("--worktree launch failed: {0}")]
    WorktreeLaunch(String),
    /// `cfg.tmux_launch` was `Some` (the user passed `--tmux`) while
    /// `cfg.worktree_launch` was `None` (no `-w`/`--worktree`). Mirrors the CLI's
    /// own `--tmux` doc ("Create a tmux session for the worktree (requires
    /// --worktree)") as a hard boot failure rather than silently ignoring the
    /// flag — worktree-tmux-launch plan Task 4.
    #[error("--tmux requires --worktree")]
    TmuxRequiresWorktree,
    /// Bare `--tmux` (the "native" mode, `tmux_launch == Some("")`) was passed
    /// on Windows. 206's native pre-flight rejects it (`Ut()==="windows" →
    /// "--tmux is not supported on Windows"`, binary @230041975). `--tmux=classic`
    /// skips this native pre-check.
    #[error("--tmux is not supported on Windows")]
    TmuxNotSupportedOnWindows,
    /// Bare `--tmux` (native mode) was passed but `tmux` is not installed
    /// (`tmux -V` non-zero). 206's native pre-flight rejects it (`!await i4i()
    /// → "tmux is not installed.\n" + s4i()`, binary @230041975). The payload is
    /// the platform-specific install hint (`s4i()`). `--tmux=classic` skips this
    /// native pre-check (a missing tmux then degrades to the non-fatal
    /// create-session warning).
    #[error("tmux is not installed.\n{0}")]
    TmuxNotInstalled(String),
}

/// Build a fully-wired desktop [`DesktopRuntime`] from a deterministic
/// [`DesktopConfig`] (F2-01 — the runtime-wiring lift out of `apps/cli`).
///
/// This is the byte-equivalent move of `apps/cli`'s `build_runtime`: every value
/// it read from `std::env`/`Argv` now arrives as an explicit `cfg` field, so
/// BOTH the CLI host and the bridge-server can construct an identical runtime
/// WITHOUT touching the process environment (the F2 end-to-end test depends on
/// this determinism). No `std::env`/`Argv` reads occur inside `build`.
///
/// The engine behavior is preserved verbatim (spec §1 non-goal — no
/// orchestrator / api-client semantic changes); only the *source* of each input
/// moved from env/argv to `cfg`, and the output/permission sinks become
/// connection-scoped parameters:
///
/// - `output` is the [`lingxi_core::host::OutputStream`] the orchestrator pushes turn
///   events to. The CLI supplies its NDJSON/plain/TUI sink; the bridge-server
///   supplies a `client::adapter::AdapterOutputStream`. The SAME `build` serves
///   both.
/// - `permission_sink` is the destination for the [`AdapterPermissionGate`]'s
///   outbound `PermissionRequest`s. It is wired ONLY when
///   `cfg.use_noop_permission_gate` is `false`; the CLI passes a sink that is
///   never used because it opts into `NoOpPermissionGate`.
///
/// Resolve the Claude.ai-subscriber flag (`isClaudeAISubscriber`, `auth.ts:1564`)
/// for a session that holds a stored OAuth token.
///
/// `isClaudeAISubscriber()` is `isAnthropicAuthEnabled() && shouldUseClaudeAIAuth(scopes)`.
/// `isAnthropicAuthEnabled()` reduces to "the auth resolver picks the stored
/// OAuth session" — `resolve` is driven with the FULL
/// [`llm_runtime::auth::anthropic::resolver::ResolverContext`] (M13), so every
/// documented ranking applies: managed OAuth forcing outranks env keys, env
/// `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_API_KEY` and an FD-inherited key outrank
/// stored OAuth, and stored OAuth outranks the stored/settings/helper/Bedrock
/// keys. When OAuth is the effective source, `shouldUseClaudeAIAuth(scopes)`
/// (== presence of the `user:inference` scope, via
/// `llm_runtime::auth::anthropic::subscription_from_scopes`) decides.
///
/// KNOWN RESIDUAL DIVERGENCE (`zb()` @228933355). The oracle suppresses on
/// TWO arms with TWO DIFFERENT host predicates:
/// ```js
/// let a = (n||i) && !KWr() || (r||s) && !YIt();   // n=AUTH_TOKEN, i=API_KEY|apiKeyHelper,
/// return !(e||a);                                //  r=settings apiKeyHelper, s=FD key
/// ```
/// `KWr()` is `YIt() && !CLAUDE_CODE_HOST_AUTH_ENV_VAR && entrypoint !== "claude-desktop-3p"`,
/// so `KWr() ⊆ YIt()`. Routing everything through the resolver's single
/// `managed_oauth_only` (the `KWr()` reading) collapses the two arms: in a
/// context where `YIt()` holds but `KWr()` does NOT — `CLAUDE_CODE_HOST_AUTH_ENV_VAR`
/// set, or the `claude-desktop-3p` entrypoint — the oracle EXEMPTS an
/// apiKeyHelper / FD-inherited key (still a subscriber) while this port
/// suppresses. The env-bearer/API-key arm (the common case, and the one AUTH-1
/// fixed) is correct. Closing this needs the resolver to carry the `YIt()`
/// reading alongside `managed_oauth_only`.
fn oauth_subscriber_flag(
    source: &llm_runtime::auth::anthropic::resolver::AuthSource,
    scopes: &[String],
) -> bool {
    matches!(
        source,
        llm_runtime::auth::anthropic::resolver::AuthSource::OAuthClaudeAi
    ) && lingxi_llm_client::auth::oauth::anthropic::subscription_from_scopes(scopes)
}

/// Seed of the shared subscription slot for a session that holds a stored
/// Claude.ai credential.
///
/// The tier persisted inside the credential is readable through
/// `getSubscriptionType()` (`Aa()` @228959617) and `getRateLimitTier()` (`jW()`,
/// same region), and BOTH short-circuit to `null` unless
/// `isAnthropicAuthEnabled()` (`zb()` @228933355) holds — i.e. a leftover stored
/// blob under an env key / bearer / FD key reports NO tier at all. So the tier
/// is gated on the RESOLVER's pick alone: `Aa()` does NOT additionally require
/// the `user:inference` scope that `isClaudeAISubscriber` folds into
/// [`oauth_subscriber_flag`], so an inference-less OAuth session still reports
/// its tier while `is_subscriber` is false.
fn subscription_seed(
    source: &llm_runtime::auth::anthropic::resolver::AuthSource,
    scopes: &[String],
    subscription_type: Option<&String>,
    rate_limit_tier: Option<&String>,
) -> lingxi_core::host::subscription::SubscriptionSnapshot {
    let oauth_effective = matches!(
        source,
        llm_runtime::auth::anthropic::resolver::AuthSource::OAuthClaudeAi
    );
    lingxi_core::host::subscription::SubscriptionSnapshot {
        is_subscriber: oauth_subscriber_flag(source, scopes),
        subscription_type: oauth_effective
            .then(|| subscription_type.cloned())
            .flatten(),
        rate_limit_tier: oauth_effective.then(|| rate_limit_tier.cloned()).flatten(),
        ..Default::default()
    }
}

/// Fold the profile + roles responses into the shared snapshot. Pure —
/// unit-tested without IO. Tier mapping mirrors
/// `OAuthProfileResponse::subscription_type()` (TS string union values);
/// `Free`/`Unknown` resolve to `None` (conservative, same as the TS `null`;
/// note `subscription_type()` never actually returns those variants today, so
/// that arm is purely defensive).
fn subscription_snapshot_from(
    is_subscriber: bool,
    profile: Option<&lingxi_llm_client::auth::oauth::anthropic::OAuthProfileResponse>,
    roles: Option<&lingxi_llm_client::auth::oauth::anthropic::UserRolesResponse>,
) -> lingxi_core::host::subscription::SubscriptionSnapshot {
    let org = profile.and_then(|p| p.organization.as_ref());
    let subscription_type = profile
        .and_then(lingxi_llm_client::auth::oauth::anthropic::subscription_type)
        .and_then(lingxi_llm_client::auth::oauth::anthropic::paid_subscription_type);
    lingxi_core::host::subscription::SubscriptionSnapshot {
        is_subscriber,
        subscription_type: subscription_type.map(str::to_owned),
        rate_limit_tier: org.and_then(|o| o.rate_limit_tier.clone()),
        has_extra_usage_enabled: org.and_then(|o| o.has_extra_usage_enabled) == Some(true),
        billing_type: org.and_then(|o| o.billing_type.clone()),
        organization_role: roles.and_then(|r| r.organization_role.clone()),
    }
}

// Phase 2a: the multi-provider client config / chains / credential sources /
// pricing catalog are now assembled by `provider_config::assemble` (which owns
// the byte-equivalent Anthropic profile + the builtin catalog presets + the
// settings-`providers` merge). The old single-Anthropic `builtin_anthropic_config`
// / `apply_settings_providers` / `parse_routing_overrides` helpers from
// `platform_common::llm_config` are no longer wired into `build()`; they remain
// in `platform_common` and are still exercised by the e2e tests below via their
// fully-qualified `platform_common::` paths.

/// (Phase 2a I1/I2) Human provider header for an assembled profile name, used to
/// label the engine's `model_providers` map the `/model` picker joins. Mirrors
/// the orchestrator catalog's `provider_label` for the built-in profiles
/// (`list_model_listings` parity) and Title-Cases an unknown USER profile name
/// (e.g. `groq` -> `Groq`, `my-provider` -> `My Provider`) so a user-defined
/// provider reads cleanly in its own group.
fn provider_profile_label(profile_name: &str) -> String {
    // A provider reachable several ways names each connection
    // `<group>:<connection>` (+ `#<n>` per extra key slot). Label it after its
    // VENDOR plus the connection, so the `/model` header reads "DeepSeek · cn"
    // rather than the title-cased id "Deepseek:cn".
    let (group, connection, slot) = lingxi_core::host::split_connection_profile(profile_name);
    if connection.is_some() || slot.is_some() {
        let base = provider_profile_label(group);
        return match (connection, slot) {
            (Some(connection), Some(slot)) => format!("{base} · {connection} · key {}", slot + 1),
            (Some(connection), None) => format!("{base} · {connection}"),
            (None, Some(slot)) => format!("{base} · key {}", slot + 1),
            (None, None) => base,
        };
    }
    match profile_name {
        "anthropic" => "Anthropic".to_string(),
        "openrouter" => "OpenRouter".to_string(),
        "deepseek" => "DeepSeek".to_string(),
        "kimi" => "Kimi".to_string(),
        "kimi-code" => "Kimi Code".to_string(),
        "glm-coding" => "GLM (coding)".to_string(),
        "zai" => "Z.AI".to_string(),
        "openai" => "OpenAI".to_string(),
        "openai-chatgpt" => "OpenAI (ChatGPT login)".to_string(),
        "github-copilot" => "GitHub Copilot".to_string(),
        other => other
            .split(['-', '_', ' '])
            .filter(|w| !w.is_empty())
            .map(|w| {
                let mut chars = w.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// First-party Anthropic models the engine routes by default, plus the
/// configured `default_model` / `fallback_model` and any env-configured
/// small-fast / haiku model a `prompt` hook may resolve to. The `llm_runtime`
/// registry resolves a request model by exact id, so every model the host may
/// request must appear here.
fn anthropic_models_for(
    default_model: &str,
    fallback_model: Option<&str>,
) -> Vec<llm_runtime::ModelProfile> {
    let caps = llm_runtime::Capabilities {
        streaming: true,
        tools: true,
        vision: true,
        documents: true,
        reasoning: true,
        structured_output: true,
    };
    let mut ids: Vec<String> = vec![
        // Opus 5 is the current first-party Opus model (Claude Code 2.1.219+).
        // Keep it in the host's exact-id registry even when the configured
        // default is Sonnet, otherwise `/model` advertises no route for it.
        "claude-opus-5".to_string(),
        "claude-opus-4-8".to_string(),
        "claude-opus-4-6".to_string(),
        "claude-opus-4-5-20251101".to_string(),
        "claude-opus-4-1-20250805".to_string(),
        "claude-opus-4-20250514".to_string(),
        // Sonnet 5 — the 2.1.198 default first-party model.
        "claude-sonnet-5".to_string(),
        "claude-sonnet-4-6".to_string(),
        "claude-sonnet-4-5-20250929".to_string(),
        "claude-haiku-4-5".to_string(),
        // Fable 5.1 replaces Fable 5 in the curated Anthropic picker.
        "claude-fable-5-1".to_string(),
    ];
    // Register the configured default/fallback under the ANTHROPIC profile ONLY
    // when it actually ROUTES to anthropic (a `claude-*` id, an unqualified
    // custom id, or an `anthropic/…` ref). A ref qualified for ANOTHER provider
    // (`openrouter/…`, `github-copilot/…`, `deepseek/…`) must NOT be added here:
    // doing so put e.g. `meta-llama/llama-3.3-70b-instruct:free` into BOTH the
    // anthropic AND openrouter model lists, so `--model <that>` failed with a
    // spurious "ambiguous across profiles: anthropic, openrouter". Push the BARE
    // model (so `anthropic/claude-x` registers as `claude-x`, not the qualified
    // ref). `split_profile_model` is the canonical routing split.
    //
    // The remainder must additionally be a BARE id. `split_profile_model` only
    // splits on the FIRST slash, so a DOUBLE-qualified ref
    // (`anthropic/deepseek/deepseek-flash` — what a client that re-qualified
    // an already-qualified id sends) yields `("anthropic",
    // "deepseek/deepseek-flash")` and used to register a DeepSeek model
    // inside the Anthropic profile, which is the same "DeepSeek V4 Flash under
    // the ANTHROPIC header" defect `harness_runtime::mobile::anthropic_models` guards. A
    // bare `anthropic/` likewise fails `split_profile_model`'s own non-empty
    // check and falls through its `_` arm, registering a model literally named
    // `anthropic/`.
    let admit = |m: &str| -> Option<String> {
        let (profile, bare) = llm_runtime::split_profile_model(m);
        (profile == "anthropic" && !bare.is_empty() && !bare.contains('/')).then_some(bare)
    };
    let fallback_models = fallback_model
        .into_iter()
        .flat_map(|csv| csv.split(','))
        .map(str::trim)
        .filter(|m| !m.is_empty());
    for m in std::iter::once(default_model).chain(fallback_models) {
        if let Some(bare) = admit(m) {
            ids.push(bare);
        }
    }
    // Env-configured small-fast / haiku model a `prompt` hook may resolve to
    // (matching `hook_prompt_runner::resolve_model`'s precedence:
    // `ANTHROPIC_SMALL_FAST_MODEL` > `ANTHROPIC_DEFAULT_HAIKU_MODEL` > default
    // Haiku), so such a request resolves instead of failing `ModelUnavailable`.
    // The default Haiku id (`claude-haiku-4-5`) is already in the list above.
    // Same admission rule: pushing the raw env value bypassed the guard above,
    // so `ANTHROPIC_SMALL_FAST_MODEL=anthropic/claude-haiku-4-5` registered the
    // QUALIFIED string as a model id and a foreign ref leaked straight in.
    for var in [
        "ANTHROPIC_SMALL_FAST_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        if let Ok(m) = std::env::var(var) {
            if let Some(bare) = admit(m.trim()) {
                ids.push(bare);
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .map(|id| llm_runtime::ModelProfile {
            display_model: id.clone(),
            request_model: id.clone(),
            billing_model: id,
            aliases: Vec::new(),
            description: None,
            metadata: Default::default(),
            capabilities: caps,
        })
        .collect()
}

/// One `settings.recentModels` entry threaded in by the host (the CLI reads the
/// file; F2-01 keeps `build()` off the filesystem for host config): a prior
/// `/model` pick, most-recent-first. `provider` is the catalog profile name;
/// `model` is the BARE wire `request_model` (the on-disk schema splits them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentModelRef {
    /// Catalog profile name (`ModelListing::provider_id`).
    pub provider: String,
    /// Bare wire model id (`ModelListing::request_model`).
    pub model: String,
}

/// Host-facing notice that the boot-time connected-provider fallback rerouted
/// the session default model. Surfaced on [`DesktopRuntime`]; both refs are in
/// display form (`profile/model`-qualified for non-anthropic routes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultModelFallbackNotice {
    /// The configured default that did NOT boot (verbatim as configured).
    pub from: String,
    /// The connected-provider model the session booted on instead.
    pub to: String,
}

/// Outcome of [`connected_provider_fallback`]: what the session boots on
/// instead of the configured (disconnected-provider) default model.
struct DefaultModelFallback {
    /// Bare wire id of the fallback model.
    model: String,
    /// Provider profile the fallback routes to — ALWAYS set (anthropic
    /// included), so the `switch_model` seeding at the end of `build()` scopes
    /// the session and a wire id that exists under several providers (e.g.
    /// `claude-sonnet-5` on anthropic AND github-copilot) resolves
    /// unambiguously instead of failing every turn with "ambiguous across
    /// profiles".
    profile: String,
}

/// Boot-time connected-provider default-model fallback (`LingXi` multi-provider
/// divergence — upstream claude-code is Anthropic-only and has no analog):
/// when the configured default model's provider is DEFINITIVELY disconnected
/// (`availability[provider] == false`; an ABSENT entry means the probe is
/// blind to that provider, so the default is conservatively kept) and at least
/// one other provider IS connected, boot on that provider instead of into
/// guaranteed first-turn auth failures.
///
/// Preference: (1) the most recent `/model` pick (`settings.recentModels`) on
/// a connected provider whose model still exists in the catalog; (2) the first
/// connected provider in [`lingxi_core::host::provider_fallback_order`], on its
/// [`lingxi_core::host::provider_default_model`]; (3) any remaining connected provider
/// (user-defined — no curated default), on its first listed model. Every
/// candidate is validated against the live `listings` so the reroute can never
/// select an id `switch_model`/the wire would reject.
///
/// `anthropic_probe_definitive` is `false` on gateway installs (a custom
/// `api_base` / `ANTHROPIC_AUTH_TOKEN` serves Claude WITHOUT a local key or
/// OAuth): there `availability["anthropic"] == false` is probe-blindness, not
/// disconnection — an anthropic-routed default is then kept as-is (the same
/// protection the TUI `/model` picker's `connected_model_rows` gives the
/// current model's provider).
fn connected_provider_fallback(
    default_model_id: &str,
    default_model_profile: Option<&str>,
    anthropic_probe_definitive: bool,
    model_providers: &std::collections::BTreeMap<String, (String, String)>,
    availability: &std::collections::BTreeMap<String, bool>,
    listings: &[lingxi_core::host::ModelListing],
    recents: &[RecentModelRef],
) -> Option<DefaultModelFallback> {
    // Effective provider of the configured default — the same resolution the
    // session_provider_first_party gate uses (explicit profile, else the
    // model_providers grouping, else the native anthropic route).
    let default_provider = default_model_profile
        .map(str::to_string)
        .or_else(|| {
            model_providers
                .get(default_model_id)
                .map(|(p, _)| p.clone())
        })
        .unwrap_or_else(|| "anthropic".to_string());
    if default_provider == "anthropic" && !anthropic_probe_definitive {
        return None; // gateway/auth-override install — the probe can't see its auth
    }
    if availability.get(default_provider.as_str()) != Some(&false) {
        return None; // connected — or the probe doesn't know this provider
    }
    let connected = |p: &str| availability.get(p) == Some(&true);
    let in_listings = |p: &str, m: &str| {
        listings
            .iter()
            .any(|l| l.provider_id == p && l.request_model == m)
    };
    let route = |model: String, provider: &str| DefaultModelFallback {
        model,
        profile: provider.to_string(),
    };
    // (1) The most recent /model pick on a connected provider.
    for r in recents {
        if connected(&r.provider) && in_listings(&r.provider, &r.model) {
            return Some(route(r.model.clone(), &r.provider));
        }
    }
    // (2) Deterministic provider order, each on its curated boot default.
    for p in lingxi_core::host::provider_fallback_order() {
        if !connected(p) {
            continue;
        }
        if let Some(m) = lingxi_core::host::provider_default_model(p) {
            if in_listings(p, m) {
                return Some(route(m.to_string(), p));
            }
        }
    }
    // (3) Any remaining connected provider (user-defined): first listed model.
    for (p, on) in availability {
        if !on {
            continue;
        }
        if let Some(l) = listings.iter().find(|l| &l.provider_id == p) {
            return Some(route(l.request_model.clone(), p));
        }
    }
    None
}

/// Load the merged `settings.outputStyle` (project + user + env layers) for the
/// given project dir. Mirrors the CLI's `load_routing`/`load_provider_profiles`
/// helpers (same `lingxi_core::settings::Settings::load` seam). Returns `None` on any
/// load failure or when the field is unset — the caller then injects no output
/// style section (OUTSTYLE.2).
#[derive(Clone)]
struct MergedSettingsCacheEntry {
    project_dir: PathBuf,
    revision: u64,
    value: lingxi_core::settings::EffectiveSettings,
}

fn merged_settings_revision(project_dir: &Path, env: &BTreeMap<String, String>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    project_dir.hash(&mut hasher);
    for path in [
        lingxi_core::settings::loader::user_settings_path(),
        Some(lingxi_core::settings::loader::project_settings_path(
            project_dir,
        )),
        Some(lingxi_core::settings::loader::local_settings_path(
            project_dir,
        )),
    ]
    .into_iter()
    .flatten()
    {
        path.hash(&mut hasher);
        match std::fs::metadata(&path) {
            Ok(metadata) => {
                metadata.len().hash(&mut hasher);
                if let Ok(modified) = metadata.modified() {
                    if let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) {
                        since_epoch.as_secs().hash(&mut hasher);
                        since_epoch.subsec_nanos().hash(&mut hasher);
                    }
                }
            }
            Err(_) => 0u8.hash(&mut hasher),
        }
    }
    for (key, value) in env {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    hasher.finish()
}

fn load_merged_settings(project_dir: &Path) -> Option<lingxi_core::settings::EffectiveSettings> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let revision = merged_settings_revision(project_dir, &env);
    let cache = SETTINGS_CACHE.get_or_init(|| Mutex::new(None));
    if let Some(entry) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|entry| entry.project_dir == project_dir && entry.revision == revision)
    {
        return Some(entry.value.clone());
    }
    let inputs = lingxi_core::settings::LoadInputs {
        env: &env,
        project_dir,
        defaults: lingxi_core::settings::schema::SettingsJson::default(),
    };
    let value = lingxi_core::settings::Settings::load(inputs).ok()?;
    *cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(MergedSettingsCacheEntry {
        project_dir: project_dir.to_path_buf(),
        revision,
        value: value.clone(),
    });
    Some(value)
}

/// Resolve the startup region with the same file, CLI, managed and env layers
/// used by the execution stack. Catalog previews must use this too.
pub fn resolve_provider_region(
    cfg: &DesktopConfig,
    managed: &BTreeMap<String, serde_json::Value>,
) -> llm_runtime::Region {
    let tiers = vec![serde_json::to_string(managed).expect("managed settings serialize")];
    match load_effective_settings_for_config(cfg, &tiers)
        .and_then(|settings| settings.settings.provider_region)
        .unwrap_or_default()
    {
        lingxi_core::settings::ProviderRegion::International => llm_runtime::Region::International,
        lingxi_core::settings::ProviderRegion::ChinaMainland => llm_runtime::Region::ChinaMainland,
    }
}

fn load_effective_settings_for_config(
    cfg: &DesktopConfig,
    managed_raw_tiers: &[String],
) -> Option<lingxi_core::settings::EffectiveSettings> {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let managed_layers: Vec<lingxi_core::settings::SettingsJson> = managed_raw_tiers
        .iter()
        .filter_map(|raw| serde_json::from_str(raw).ok())
        .collect();
    let (include_user, include_project) = if cfg.restricted {
        (false, false)
    } else {
        cfg.setting_source_scope
    };
    lingxi_core::settings::Settings::load_with_layers_from_user_path(
        lingxi_core::settings::LoadInputs {
            env: &env,
            project_dir: &cfg.cwd,
            defaults: lingxi_core::settings::schema::SettingsJson::default(),
        },
        lingxi_core::settings::FileLayerScope {
            include_user,
            include_project,
            include_local: include_project,
        },
        lingxi_core::settings::SupplementalLayers {
            cli_layer: cfg.flag_settings.as_ref(),
            managed_layers: &managed_layers,
        },
        Some(&cfg.lingxi_home.join("settings.json")),
    )
    .ok()
}

/// `Bk("bashEditDiffEnabled")[0]` — the value as the USER / flag / policy tiers
/// alone see it, ignoring project and project-local settings.
///
/// 🚨 This is NOT `effective_settings.settings.bash_edit_diff_enabled`, and it
/// is not recoverable from the provenance trace either: for an `Override` field
/// the trace keeps only the WINNING layer, so a user `true` under a project
/// `false` leaves no trace entry naming the user tier. The tier has to be
/// resolved by loading the tier stack on its own.
///
/// The distinction is the security property of `y0r`: a `true` from these tiers
/// turns the feature on in any mode, so a checked-in `.lingxi/settings.json`
/// must not be able to reach that arm. A `false` from ANY layer still wins,
/// which is why the merged value is read separately and both are passed to
/// [`tool_shell::bash_edit_diff::enabled`].
fn load_trusted_tier_bash_edit_diff(
    cfg: &DesktopConfig,
    managed_raw_tiers: &[String],
) -> Option<bool> {
    let env: BTreeMap<String, String> = BTreeMap::new();
    let managed_layers: Vec<lingxi_core::settings::SettingsJson> = managed_raw_tiers
        .iter()
        .filter_map(|raw| serde_json::from_str(raw).ok())
        .collect();
    let include_user = !cfg.restricted && cfg.setting_source_scope.0;
    lingxi_core::settings::Settings::load_with_layers_from_user_path(
        lingxi_core::settings::LoadInputs {
            // Deliberately EMPTY: `CLAUDE_CODE_BASH_EDIT_DIFF` is the gate's
            // first arm and is read there. Letting the env layer contribute
            // here would make an env value look like a policy tier.
            env: &env,
            project_dir: &cfg.cwd,
            defaults: lingxi_core::settings::schema::SettingsJson::default(),
        },
        lingxi_core::settings::FileLayerScope {
            include_user,
            include_project: false,
            include_local: false,
        },
        lingxi_core::settings::SupplementalLayers {
            cli_layer: cfg.flag_settings.as_ref(),
            managed_layers: &managed_layers,
        },
        Some(&cfg.lingxi_home.join("settings.json")),
    )
    .ok()
    .and_then(|effective| effective.settings.bash_edit_diff_enabled)
}

/// Resolve CLI-5's gate and, when it is on, the per-session shadow root.
///
/// `None` IS the off state — `BuiltinToolContext::bash_edit_diff` carries no
/// separate boolean, so a host that never calls this behaves exactly as before.
///
/// The shadow root is per SESSION (oracle `eNt` names each shadow
/// `<session>-<repo>-<random>` under a 0700 cache dir), so two sessions working
/// the same checkout never share one shadow index.
fn resolve_bash_edit_diff(
    cfg: &DesktopConfig,
    effective_settings: Option<&lingxi_core::settings::EffectiveSettings>,
    managed_raw_tiers: &[String],
    session_id: &str,
) -> Option<Arc<tool_api::builtin_context::BashEditDiffSetup>> {
    let env_override = std::env::var("CLAUDE_CODE_BASH_EDIT_DIFF")
        .ok()
        .map(|raw| !matches!(raw.trim(), "" | "0" | "false"));
    // `Aot()`: the env var when defined, else the `tengu_thrifty_sonic` cohort,
    // which defaults FALSE. So the "on in auto mode" arm is DEAD in an
    // unconfigured install — upstream too. The feature is opt-in.
    let rollout = std::env::var("CLAUDE_CODE_THRIFTY_SONIC").map_or_else(
        |_| telemetry::flag_bool("tengu_thrifty_sonic", false),
        |raw| !matches!(raw.trim(), "" | "0" | "false"),
    );
    resolve_bash_edit_diff_with(
        cfg,
        effective_settings,
        managed_raw_tiers,
        session_id,
        env_override,
        rollout,
    )
}

/// [`resolve_bash_edit_diff`] with its two process-global env reads lifted out.
///
/// ⚠️ The split is not cosmetic: a gate that reads `std::env` cannot be tested
/// without `set_var`, which is process-global and flakes the parallel suite —
/// and the resulting failure looks like a concurrent session's fault. Resolve
/// the env at the edge, pass it as a parameter.
fn resolve_bash_edit_diff_with(
    cfg: &DesktopConfig,
    effective_settings: Option<&lingxi_core::settings::EffectiveSettings>,
    managed_raw_tiers: &[String],
    session_id: &str,
    env_override: Option<bool>,
    rollout: bool,
) -> Option<Arc<tool_api::builtin_context::BashEditDiffSetup>> {
    let trusted_tier = load_trusted_tier_bash_edit_diff(cfg, managed_raw_tiers);
    let merged = effective_settings.and_then(|e| e.settings.bash_edit_diff_enabled);
    let permissive = matches!(
        cfg.permission_mode,
        permission::PermissionMode::Auto | permission::PermissionMode::BypassPermissions
    );
    if !tool_shell::bash_edit_diff::enabled(env_override, trusted_tier, merged, permissive, rollout)
    {
        return None;
    }
    Some(Arc::new(tool_api::builtin_context::BashEditDiffSetup {
        shadow_root: cfg.lingxi_home.join("bash-edit-diff").join(session_id),
    }))
}

static SETTINGS_CACHE: OnceLock<Mutex<Option<MergedSettingsCacheEntry>>> = OnceLock::new();

fn load_merged_output_style(project_dir: &std::path::Path) -> Option<String> {
    load_merged_settings(project_dir).and_then(|eff| eff.settings.output_style)
}

/// Load the merged `settings.showThinkingSummaries` request-beta preference.
/// Absent/invalid settings resolve to Claude Code's default (`false`).
fn load_merged_show_thinking_summaries(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .and_then(|eff| eff.settings.show_thinking_summaries)
        .unwrap_or(false)
}

/// Load LingXi's `settings.visionDelegationEnabled` preference. The feature is
/// enabled when absent so existing installations gain the safe image sidecar
/// without a migration.
fn load_merged_vision_delegation_enabled(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .and_then(|effective| effective.settings.vision_delegation_enabled)
        .unwrap_or(true)
}

/// Load the merged `settings.agentPushNotifEnabled` preference. The independent
/// `tengu_kairos_push_notifications` feature flag is applied by consumers.
fn load_merged_agent_push_notif_enabled(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .and_then(|effective| effective.settings.agent_push_notif_enabled)
        .unwrap_or(false)
}

/// Load the merged `settings.taskOutputMaxChars`. `None` when unset — the
/// oracle's `Ge().taskOutputMaxChars === undefined` branch, which is what makes
/// `TASK_MAX_OUTPUT_LENGTH` apply.
fn load_merged_task_output_max_chars(project_dir: &std::path::Path) -> Option<u32> {
    load_merged_settings(project_dir).and_then(|eff| eff.settings.task_output_max_chars)
}

/// Load the merged `settings.bashOutputMaxChars`. `None` when unset — the
/// branch under which `BASH_MAX_OUTPUT_LENGTH` applies.
fn load_merged_bash_output_max_chars(project_dir: &std::path::Path) -> Option<u32> {
    load_merged_settings(project_dir).and_then(|eff| eff.settings.bash_output_max_chars)
}

/// Load `settings.attribution` + `settings.includeCoAuthoredBy` — the git
/// attribution trailer overrides, resolved the same way as the caps above.
fn load_merged_attribution(
    project_dir: &std::path::Path,
) -> (Option<String>, Option<String>, Option<bool>) {
    let Some(eff) = load_merged_settings(project_dir) else {
        return (None, None, None);
    };
    let attribution = eff.settings.attribution.clone();
    (
        attribution.as_ref().and_then(|a| a.commit.clone()),
        attribution.and_then(|a| a.pr),
        eff.settings.include_co_authored_by,
    )
}

/// Load `settings.workflowKeywordTriggerEnabled`. The default remains off,
/// matching Claude Code's optional setting.
fn load_merged_workflow_keyword_trigger_enabled(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .and_then(|eff| eff.settings.workflow_keyword_trigger_enabled)
        .unwrap_or(false)
}

/// Load the merged `settings.skipWebFetchPreflight` (project + user + env layers)
/// for the given project dir. Mirrors [`load_merged_output_style`] (same
/// `lingxi_core::settings::Settings::load` seam). When true, the `WebFetch` tool skips
/// the domain-blocklist preflight (CC 2.1.207 `!Mi().skipWebFetchPreflight` gate,
/// parity P2-14). Returns `false` on any load failure or when the key is unset —
/// the frozen default (preflight runs).
fn load_merged_skip_web_fetch_preflight(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .and_then(|eff| eff.settings.skip_web_fetch_preflight)
        .unwrap_or(false)
}

/// Load the merged `settings.disableAgentView` (project + user + env layers) for
/// the given project dir. Mirrors [`load_merged_skip_web_fetch_preflight`] (same
/// `lingxi_core::settings::Settings::load` seam). When `true`, the agent-view
/// fork/subtask surface is disabled exactly like `CLAUDE_CODE_DISABLE_AGENT_VIEW=1`
/// (binary `I2i()` — `settings.disableAgentView === true`), threaded into
/// [`command_api::builtins::register_core_batch_8`] via
/// [`lingxi_core::host::agent_view::is_enabled_with_setting`] (M-03). Returns `false` on any
/// load failure or when the key is unset — the frozen default (agent view
/// enabled; the env half still applies independently).
fn load_merged_disable_agent_view(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .and_then(|eff| eff.settings.disable_agent_view)
        .unwrap_or(false)
}

/// Resolve the merged hooks-restricted flag for the `/goal` gate (review #12):
/// `disableAllHooks || allowManagedHooksOnly` across the project/user/env layers
/// (the same `Settings::load` seam). Mirrors claude's `kEt` hooks half —
/// `if (tX() || lMe()) return hooks_gate`. `false` on any load failure or when
/// both keys are unset (the permissive default).
fn load_merged_hooks_restricted(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .map(|eff| {
            eff.settings.disable_all_hooks.unwrap_or(false)
                || eff.settings.allow_managed_hooks_only.unwrap_or(false)
        })
        .unwrap_or(false)
}

/// Resolve the effective `disableAllHooks` value for the executor's runner-head
/// kill switch. Unlike the `/goal` restriction helper above, this returns only
/// the setting that suppresses every hook, including hooks registered after
/// boot.
fn load_merged_disable_all_hooks(project_dir: &std::path::Path) -> bool {
    load_merged_settings(project_dir)
        .and_then(|eff| eff.settings.disable_all_hooks)
        .unwrap_or(false)
}

/// The settings an administrator pinned through the managed (policy) layer:
/// key → value, read from the SAME file-based tiers the merge above consumes
/// (`managed_settings_raw_tiers`).
///
/// Returns the VALUES, not just the key names, because a consumer that only
/// knew the keys would have to report some lower layer's value for exactly the
/// keys the lower layer cannot win — managed outranks every file layer in the
/// engine's precedence (`env → managed → cli → local → project → user →
/// defaults`). A settings UI given only the keys renders the wrong current
/// value and puts a padlock next to it.
///
/// Tier precedence is preserved from `managed_settings_raw_tiers`, which
/// returns tiers in ASCENDING priority: a later tier's key overwrites an
/// earlier one's, so `managed-settings.d` drop-ins win over the base
/// `managed-settings.json`.
///
/// Lives here, not in `bridge-server`, because managed-layer discovery is the
/// composition root's job — the platform-specific managed directory and its
/// drop-in ordering are resolved once, in one place. Keys are the raw JSON
/// names (the wire spelling), so they line up with the keys a settings
/// snapshot reports.
#[must_use]
pub async fn managed_settings_overlay() -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut overlay: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    for raw in crate::desktop::settings_watch::managed_settings_raw_tiers().await {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            for (key, value) in map {
                overlay.insert(key, value);
            }
        }
    }
    overlay
}

/// Resolve `askUserQuestionTimeout` from its allowed sources only: user,
/// `--settings`, and managed policy. Project/local files are intentionally
/// excluded because an untrusted checkout must not control interaction timing.
async fn load_ask_user_question_timeout(cfg: &DesktopConfig) -> Option<String> {
    let managed_layers: Vec<lingxi_core::settings::SettingsJson> =
        crate::desktop::settings_watch::managed_settings_raw_tiers()
            .await
            .into_iter()
            .filter_map(|raw| serde_json::from_str(&raw).ok())
            .collect();
    let empty_env = std::collections::BTreeMap::new();
    lingxi_core::settings::Settings::load_with_layers_from_user_path(
        lingxi_core::settings::LoadInputs {
            env: &empty_env,
            project_dir: &cfg.cwd,
            defaults: lingxi_core::settings::SettingsJson::default(),
        },
        lingxi_core::settings::FileLayerScope {
            include_user: cfg.setting_source_scope.0,
            include_project: false,
            include_local: false,
        },
        lingxi_core::settings::SupplementalLayers {
            cli_layer: cfg.flag_settings.as_ref(),
            managed_layers: &managed_layers,
        },
        Some(&cfg.lingxi_home.join("settings.json")),
    )
    .ok()
    .and_then(|effective| effective.settings.ask_user_question_timeout)
}

/// Load the merged HTTP-hook security policy (H-BIN-12) — `allowedHttpHookUrls`
/// and `httpHookAllowedEnvVars` — across the project + user + env settings
/// layers. Both are array-merge (concat-dedup) via the same
/// `lingxi_core::settings::Settings::load` seam. `(None, None)` on any load failure or
/// when neither key is set (⇒ no restriction; the HTTP hook executor behaves
/// exactly as before). Threaded into the executor via
/// [`hooks::HookExecutorImpl::with_http_hook_policy`], mirroring CC's live
/// `PFy()=Wn()` read (lingxi sources once at boot).
fn load_merged_http_hook_policy(
    project_dir: &std::path::Path,
) -> (Option<Vec<String>>, Option<Vec<String>>) {
    match load_merged_settings(project_dir) {
        Some(eff) => (
            eff.settings.allowed_http_hook_urls,
            eff.settings.http_hook_allowed_env_vars,
        ),
        None => (None, None),
    }
}

/// (P2-02 cc2.1.207) `g9e(source)` — is the agent source in the trusted set
/// `qXh`? The binary's set is
/// `new Set(["plugin","policySettings","built-in","builtin","bundled"])`, so a
/// trusted source bypasses a `strictPluginOnlyCustomization` (`uA`) restriction
/// when registering the agent's frontmatter hooks as `mainThreadAgentHooks`
/// (`Rft`). LingXi's [`agent::AgentSource`] maps (see
/// `agent::handle::agent_source_to_claude_str`): `BuiltIn`→"built-in",
/// `Plugin`→"plugin", `PolicySettings`→"policySettings" are the trusted three;
/// `UserDefined`/`Project`/`Flag`/`AdditionalDirectory` are NOT. (LingXi has no
/// "bundled" source.)
fn agent_source_is_trusted(source: agent::AgentSource) -> bool {
    matches!(
        source,
        agent::AgentSource::BuiltIn
            | agent::AgentSource::Plugin
            | agent::AgentSource::Settings(lingxi_core::types::SettingsScope::Managed)
    )
}

/// (M4 cc2.1.198) Merge the `--agents <json>` flag agents into the dir-loaded
/// catalog. The flag payload is an EXPLICIT request: it survives `--bare` but
/// not safe mode (binary @223080769 `if(r&&!Hc("agents",{explicitlyRequested:
/// !0}))try{let g=Ba(r);if(g)m=QXt(g,"flagSettings")}catch(g){De(g)}else
/// if(r)C("--agents: ignored in safe mode (user-supplied custom agents are
/// disabled)",{level:"warn"})`). Merge precedence per `XXt`'s tier map
/// `[built-in, plugin, userSettings, projectSettings, flagSettings,
/// policySettings]` (later wins): a flag agent REPLACES a same-named
/// user/project dir agent, else appends. Parse failures inside
/// [`agent::parse_agents_from_flag_json`] log and contribute no agents —
/// the flag never aborts boot.
fn merge_cli_flag_agents(
    agents: &mut Vec<agent::AgentDefinition>,
    cli_agents_json: Option<&str>,
    safe_mode: bool,
) {
    let Some(raw) = cli_agents_json else { return };
    if safe_mode {
        tracing::warn!("--agents: ignored in safe mode (user-supplied custom agents are disabled)");
        return;
    }
    for a in agent::parse_agents_from_flag_json(raw) {
        if let Some(slot) = agents.iter_mut().find(|e| e.agent_type == a.agent_type) {
            *slot = a;
        } else {
            agents.push(a);
        }
    }
}

/// (M7 cc2.1.220) Boot gates for [`merge_agent_frontmatter_mcp_servers`],
/// resolved by the composition root (env/flag safe mode, `--strict-mcp-config`,
/// `managed-mcp.json` presence) and injected so the merge is a pure,
/// unit-testable function.
#[derive(Debug, Clone, Copy)]
struct AgentMcpMergeGates {
    /// claude `Gl()` — `CLAUDE_CODE_SAFE_MODE` env truthy or `--safe-mode`.
    safe_mode: bool,
    /// claude `r?.strictMcpConfig` — the `--strict-mcp-config` CLI flag.
    strict_mcp_config: bool,
    /// claude `T3()` — a managed `managed-mcp.json` takes EXCLUSIVE control of
    /// the MCP server set; agent frontmatter servers never merge.
    enterprise_mcp_active: bool,
    /// Managed `strictPluginOnlyCustomization` lock for the MCP slot.
    strict_plugin_only_mcp: bool,
}

/// (M7 cc2.1.220) claude `FWt(existing, agentDef, opts)` @245974724 — merge the
/// resolved main-thread agent's frontmatter `mcpServers` into the to-connect
/// MCP config list, so they register + connect exactly like `--mcp-config`
/// servers. Returns the enterprise-BLOCKED server names for the caller's
/// `onBlocked` stderr warning (only the composition root prints — claude's
/// TUI/resume `FWt` call sites pass no `onBlocked`).
///
/// Gate order, byte-faithful to `FWt`:
/// 1. no agent definition → no-op (`if(!t)return e`);
/// 2. safe mode → no-op (`if(Gl())return e`);
/// 3. `--strict-mcp-config` UNLESS the agent came from `--agents`
///    (`r?.strictMcpConfig && t.source !== "flagSettings"`), OR a managed MCP
///    config is active (`|| T3()`) → no-op;
/// 4. convert via `obs` ([`agent::agent_mcp_specs_to_scoped_configs`]);
/// 5. `Yee` enterprise allow/deny filter (sdk-type always allowed) → blocked
///    names collected;
/// 6. `{...allowed, ...existing}` — but `existing` there is `dynamicMcpConfig`
///    ALONE, not the whole to-connect set. `Ot` is seeded `{}` (@245992990) and
///    only ever accumulates `--mcp-config` / Chrome / Computer-Use entries;
///    `afe`'s discovered map (`Object.assign({}, plugin, user, project, local)`
///    @231823400) deliberately excludes it. The headless site then spreads the
///    dynamic bucket LAST — `po = {...an, ...Uo}` @246008983 — so an agent
///    server BEATS a same-named discovered `.mcp.json`/user/local server and
///    loses only to a `--mcp-config` one. `dynamic_names` is that bucket's key
///    set, which this port cannot recover from the flattened list (CLI servers
///    are parsed at `ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)`).
fn merge_agent_frontmatter_mcp_servers(
    existing: &mut Vec<mcp::McpServerConfig>,
    dynamic_names: &[String],
    def: Option<&agent::AgentDefinition>,
    gates: AgentMcpMergeGates,
    policy: &mcp::enterprise_policy::McpPolicy,
) -> Vec<String> {
    let Some(def) = def else {
        return Vec::new();
    };
    if gates.safe_mode {
        return Vec::new();
    }
    if (gates.strict_mcp_config && def.source != agent::AgentSource::Flag)
        || gates.enterprise_mcp_active
    {
        return Vec::new();
    }
    // claude `Zx(r)`: a by-name entry resolves against whatever the session
    // already has discovered/configured — the SAME `existing` list this
    // merge folds INTO, snapshotted before the mutation loop below.
    let scoped = agent::agent_mcp_specs_to_scoped_configs(
        def,
        gates.strict_plugin_only_mcp,
        gates.strict_mcp_config,
        existing.as_slice(),
    );
    let mut blocked = Vec::new();
    for scoped_cfg in scoped {
        let cfg = scoped_cfg.config;
        // `Yee` — enterprise allow/deny per server (sdk short-circuit inside).
        if !mcp::enterprise_policy::is_server_allowed(&cfg, policy) {
            blocked.push(cfg.name);
            continue;
        }
        match existing.iter_mut().find(|x| x.name == cfg.name) {
            // `{...allowed, ...dynamic}`: a `--mcp-config` server of this name
            // was spread after the agent's, so it wins.
            Some(_) if dynamic_names.iter().any(|n| n == &cfg.name) => {}
            // `{...discovered, ...dynamic}`: the agent's server REPLACES the
            // discovered entry, transport spec and all.
            Some(slot) => *slot = cfg,
            None => existing.push(cfg),
        }
    }
    blocked
}

/// Owns the MCP cleanup handles [`build_agent_mcp_tool_set`]'s connect loop
/// has accumulated SO FAR, for as long as the loop is still running.
///
/// [Round-5 review item 11, class member (1) — handed here by that fixer's
/// `needs_other_file`.] `agent::handle`'s own `McpCleanupGuard` can only be
/// armed once this function RETURNS, and the loop below suspends on a real
/// dial (`connect_agent_scoped` / `connect`) once per server. A caller that
/// drops the spawn future mid-loop (the Fusion `join_set.abort_all()` race,
/// one await earlier than findings 11 and 19) therefore dropped a plain local
/// `Vec` that no caller had ever seen — every server already connected in
/// this loop leaked its live connection with nothing left able to tear it
/// down. The guard is armed before the first iteration and handed on, via
/// [`AgentMcpConnectLoopGuard::take`], only in the expression that builds the
/// returned [`agent::agent_mcp_tools::AgentMcpToolSet`].
///
/// Its `Drop` mirrors `agent::handle::McpCleanupGuard`'s: best-effort teardown
/// on the current runtime, nothing to do once no runtime is left.
struct AgentMcpConnectLoopGuard {
    lease: Option<agent::agent_mcp_tools::AgentMcpConstructionLease>,
    cleanups: Vec<agent::agent_mcp_tools::AgentMcpCleanupHandle>,
    agent_type: String,
}

impl AgentMcpConnectLoopGuard {
    fn new(
        agent_type: String,
        lease: Option<agent::agent_mcp_tools::AgentMcpConstructionLease>,
    ) -> Self {
        Self {
            lease,
            cleanups: Vec::new(),
            agent_type,
        }
    }

    fn push(&mut self, handle: agent::agent_mcp_tools::AgentMcpCleanupHandle) {
        self.cleanups.push(handle);
    }

    /// Hand the handles to their next owner. Call this ONLY in the expression
    /// that immediately consumes them; the guard is left empty, so from here
    /// on its `Drop` is a no-op.
    fn take(&mut self) -> Vec<agent::agent_mcp_tools::AgentMcpCleanupHandle> {
        std::mem::take(&mut self.cleanups)
    }
}

impl Drop for AgentMcpConnectLoopGuard {
    fn drop(&mut self) {
        if self.cleanups.is_empty() {
            return;
        }
        let cleanups = std::mem::take(&mut self.cleanups);
        let lease = self.lease.take();
        let agent_type = std::mem::take(&mut self.agent_type);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                // Moves the guard into this scope so it drops at the END of it. The lint sees
                // a `_`-binding with no side effect; the side effect is the Drop deadline.
                #[allow(clippy::no_effect_underscore_binding)]
                let _lease = lease;
                agent::agent_mcp_tools::run_agent_mcp_cleanups(cleanups, &agent_type).await;
            });
        }
    }
}

/// §24b (claude `Agr`, 2.1.251 @~160977000): connect + build ONE subagent
/// spawn's per-agent inline `mcpServers` tools. Reuses the SAME `PRn`
/// conversion as the main-thread-agent merge above
/// ([`agent::agent_mcp_specs_to_scoped_configs`]) against a snapshot of the
/// registry's LIVE connected servers (`existing_configs`, for by-name
/// resolution). An inline RECORD entry (`is_newly_created`) connects under an
/// agent-scoped table key ([`mcp::McpRegistry::connect_agent_scoped`]) so
/// concurrent spawns declaring the same plain server name never clobber each
/// other, and its tools are built with `MCPTool::bound_server_key` set to that
/// key; a by-name entry connects (or reuses) through the ordinary shared path
/// with no scoping, exactly like every other session-level server. A connect
/// failure is logged with claude's exact copy and drops only that one
/// server's tools — never fatal to the spawn.
async fn build_agent_mcp_tool_set(
    mcp_registry: Arc<mcp::McpRegistry>,
    mcp_tool_ctx: tool_api::BuiltinToolContext,
    strict_plugin_only_mcp: bool,
    strict_mcp_config: bool,
    agent_id: lingxi_core::types::AgentId,
    def: agent::AgentDefinition,
    lease: Option<agent::agent_mcp_tools::AgentMcpConstructionLease>,
) -> agent::agent_mcp_tools::AgentMcpToolSet {
    if def.mcp_servers.is_empty() {
        return agent::agent_mcp_tools::AgentMcpToolSet::default();
    }
    let existing_configs: Vec<mcp::McpServerConfig> = {
        let conns = mcp_registry.connections.read().await;
        conns.values().map(|s| s.config().clone()).collect()
    };
    let scoped = agent::agent_mcp_specs_to_scoped_configs(
        &def,
        strict_plugin_only_mcp,
        strict_mcp_config,
        &existing_configs,
    );
    let mut tools: Vec<Arc<dyn tool_api::Tool>> = Vec::new();
    // Armed BEFORE the first dial: every await inside the loop is a window in
    // which the caller can drop this future (round-5 review item 11).
    let mut cleanups = AgentMcpConnectLoopGuard::new(def.agent_type.clone(), lease);
    for entry in scoped {
        let plain_name = entry.config.name.clone();
        let config_role = entry.config.metadata.role;
        // Keep the config-side permission declarations before the connection
        // call consumes the scoped entry. The same declarations must reach
        // both shared and agent-scoped per-tool builders.
        let entry_config = entry.config.clone();
        let (table_key, bound_key): (String, Option<String>) = if entry.is_newly_created {
            match mcp_registry
                .connect_agent_scoped(entry.config, agent_id)
                .await
            {
                Ok((_, key)) => (key.clone(), Some(key)),
                Err(error) => {
                    tracing::warn!(
                        "[Agent: {}] Failed to connect to MCP server '{}': {}",
                        def.agent_type,
                        plain_name,
                        error
                    );
                    continue;
                }
            }
        } else {
            match mcp_registry.connect(entry.config).await {
                Ok(_) => (plain_name.clone(), None),
                Err(error) => {
                    tracing::warn!(
                        "[Agent: {}] Failed to connect to MCP server '{}': {}",
                        def.agent_type,
                        plain_name,
                        error
                    );
                    continue;
                }
            }
        };
        // Record ownership before the first post-connect await. Cancellation
        // while waiting for the catalog read must still close this connection.
        if entry.is_newly_created {
            let cleanup_registry = mcp_registry.clone();
            let cleanup_key = table_key.clone();
            cleanups.push(agent::agent_mcp_tools::AgentMcpCleanupHandle {
                server_name: plain_name.clone(),
                run: Arc::new(move || {
                    let registry = cleanup_registry.clone();
                    let key = cleanup_key.clone();
                    Box::pin(async move {
                        registry
                            .disconnect_agent_scoped(&key)
                            .await
                            .map_err(|error| error.to_string())
                    })
                }),
            });
        }
        let dtos: Vec<lingxi_core::host::McpToolDto> = {
            let conns = mcp_registry.connections.read().await;
            match conns.get(&table_key) {
                // §11 Stage 2: `connect`/`connect_agent_scoped` above may have
                // resolved a discovery-cache hit instead of dialing — the
                // server is `Cached`, not `Connected`, but carries the same
                // catalog, so the subagent's tool set must be built from it
                // exactly the same way.
                Some(
                    mcp::McpConnectionState::Connected { tools, .. }
                    | mcp::McpConnectionState::Cached { tools, .. },
                ) => tools.clone(),
                _ => Vec::new(),
            }
        };
        tracing::info!(
            "[Agent: {}] Connected to MCP server '{}' with {} tools",
            def.agent_type,
            plain_name,
            dtos.len()
        );
        for dto in &dtos {
            let tool = tool_mcp::MCPTool::new_for_tool(
                mcp_tool_ctx.clone(),
                dto.full_name.clone(),
                dto.description.clone(),
                dto.input_schema.clone(),
                None,
                None,
                dto.search_hint.clone(),
                dto.always_load.unwrap_or(false),
                dto.requires_user_interaction,
            );
            let tool = if let Some(ceiling) =
                tool_mcp::configured_permission_ceiling(&entry_config, &dto.tool_name)
            {
                tool.with_mcp_permission_ceiling(ceiling)
            } else {
                tool
            };
            let tool = match &bound_key {
                Some(key) => tool.with_bound_server_key(key.clone()),
                None => tool,
            };
            let tool = tool.with_mcp_role(
                (config_role == Some(mcp::McpServerRole::Comms)).then(|| "comms".to_string()),
            );
            tools.push(Arc::new(tool) as Arc<dyn tool_api::Tool>);
        }
    }
    agent::agent_mcp_tools::AgentMcpToolSet {
        tools,
        cleanups: cleanups.take(),
    }
}

/// Read the merged `settings.enabledPlugins` allowlist (`plugin@marketplace` →
/// enabled). Ambient user/project roots are optional; restricted sessions pass
/// `include_ambient = false` and therefore receive only explicit flagSettings
/// and managed policy entries. Mirrors `loadPluginsFromMarketplaces`'s
/// `{...getAddDirEnabledPlugins(), ...settings.enabledPlugins}` merge
/// (`pluginLoader.ts:1898`) at the priority that matters for the cache-only
/// boot. Malformed files / a missing key degrade to an empty map (no plugins),
/// matching claude-code's resilient read-only boot.
async fn load_enabled_plugins(
    lingxi_home: &std::path::Path,
    cwd: &std::path::Path,
    additional_project_roots: &[std::path::PathBuf],
    include_ambient: bool,
    flag_settings: Option<&lingxi_core::settings::SettingsJson>,
) -> std::collections::BTreeMap<String, bool> {
    let mut merged: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
    if include_ambient {
        let user = lingxi_home.join("settings.json");
        let project = cwd.join(branding::DOT_DIR).join("settings.json");
        // User first, project second → project overrides on identical keys.
        let mut paths = vec![user, project];
        paths.extend(
            additional_project_roots
                .iter()
                .map(|root| root.join(branding::DOT_DIR).join("settings.json")),
        );
        for path in paths {
            let Ok(raw) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
                tracing::warn!(path = %path.display(), "skipping malformed settings.json for enabledPlugins");
                continue;
            };
            if let Some(map) = json.get("enabledPlugins").and_then(|v| v.as_object()) {
                for (k, v) in map {
                    if let Some(b) = v.as_bool() {
                        merged.insert(k.clone(), b);
                    }
                }
            }
        }
    }
    // Explicit `--settings` is trusted even when ambient settings are
    // suppressed. It has higher precedence than ambient files and lower than
    // managed policy, matching the canonical settings tier order.
    if let Some(settings) = flag_settings {
        if let Some(enabled) = settings.enabled_plugins.as_ref() {
            for (plugin, active) in enabled {
                if let Some(active) = active.as_bool() {
                    merged.insert(plugin.clone(), active);
                }
            }
        }
    }

    // Managed policy is always eligible, including when restricted mode has
    // disabled all ambient file settings.
    for raw in crate::desktop::settings_watch::managed_settings_raw_tiers().await {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        if let Some(map) = json.get("enabledPlugins").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(b) = v.as_bool() {
                    merged.insert(k.clone(), b);
                }
            }
        }
    }
    merged
}

/// Read the merged `settings.pluginConfigs` scope (`plugin → {options,
/// mcpServers}`). Restricted sessions skip the user settings file, while the
/// explicit flagSettings and managed policy tiers remain eligible. This is the
/// composition-root READ that seeds
/// [`plugin::PluginManager::with_plugin_configs`]; malformed files / a missing
/// key degrade to an empty map (no persisted config), matching resilient boot.
async fn load_plugin_configs(
    lingxi_home: &std::path::Path,
    restricted: bool,
    flag_settings: Option<&lingxi_core::settings::SettingsJson>,
) -> std::collections::HashMap<String, plugin::PluginUserConfig> {
    let mut merged: std::collections::HashMap<String, plugin::PluginUserConfig> =
        std::collections::HashMap::new();
    if !restricted {
        let user = lingxi_home.join("settings.json");
        if let Ok(raw) = tokio::fs::read_to_string(&user).await {
            if let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&raw)
            {
                for (plugin, cfg) in plugin::PluginUserConfig::from_settings_map(&map) {
                    merged.insert(plugin, cfg);
                }
            }
        }
    }
    if let Some(settings) = flag_settings.and_then(|settings| serde_json::to_value(settings).ok()) {
        if let serde_json::Value::Object(map) = settings {
            for (plugin, cfg) in plugin::PluginUserConfig::from_settings_map(&map) {
                merged.insert(plugin, cfg);
            }
        }
    }
    for raw in crate::desktop::settings_watch::managed_settings_raw_tiers().await {
        let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw)
        else {
            continue;
        };
        for (plugin, cfg) in plugin::PluginUserConfig::from_settings_map(&map) {
            merged.insert(plugin, cfg);
        }
    }
    merged
}

/// Read the managed-only enabled plugin names (name-part only) used to recover
/// org-policy provenance for plugin session telemetry.
async fn load_managed_plugin_names() -> std::collections::HashSet<String> {
    let mut managed = std::collections::HashSet::new();
    for raw in crate::desktop::settings_watch::managed_settings_raw_tiers().await {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(map) = json.get("enabledPlugins").and_then(|v| v.as_object()) else {
            continue;
        };
        for (key, value) in map {
            if !value.as_bool().unwrap_or(false) {
                continue;
            }
            let name = key.split('@').next().unwrap_or(key);
            if !name.is_empty() {
                managed.insert(name.to_string());
            }
        }
    }
    managed
}

/// Read the managed-only blocked marketplace policy (`blockedMarketplaces`),
/// last-write-wins across the managed tiers.
async fn load_blocked_marketplaces() -> std::collections::HashSet<String> {
    let mut blocked = std::collections::HashSet::new();
    for raw in crate::desktop::settings_watch::managed_settings_raw_tiers().await {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(entries) = value.get("blockedMarketplaces").and_then(|v| v.as_array()) else {
            continue;
        };
        blocked = entries
            .iter()
            .filter_map(|entry| entry.as_str())
            .filter(|entry| !entry.is_empty())
            .map(ToOwned::to_owned)
            .collect();
    }
    blocked
}

/// Discover the set of plugins that should be active for the current session —
/// the shared body of both the startup bootstrap (§6.5) and the
/// `/reload-plugins` refresh ([`PluginRuntime::refresh`]). `ambient` resolves
/// the `enabledPlugins` allowlist against the on-disk plugin cache (with a
/// flat-walk fallback for dev/local dirs); `inline` appends any `--plugin-dir`
/// session plugins. Returns `(id, manifest, install_dir)` per plugin — exactly
/// what [`plugin::PluginManager::enable`] consumes.
async fn discover_plugin_set(
    ambient: bool,
    inline: bool,
    lingxi_home: &std::path::Path,
    cwd: &std::path::Path,
    plugins_dir: &std::path::Path,
    cli_plugin_dirs: &[std::path::PathBuf],
    additional_project_roots: &[std::path::PathBuf],
    restricted: bool,
    flag_settings: Option<&lingxi_core::settings::SettingsJson>,
    analytics_bus: &Arc<telemetry::AnalyticsBus>,
) -> Vec<(
    lingxi_core::types::PluginId,
    plugin::PluginManifest,
    std::path::PathBuf,
)> {
    let mut discovered = if ambient || restricted || flag_settings.is_some() {
        let enabled = load_enabled_plugins(
            lingxi_home,
            cwd,
            additional_project_roots,
            ambient && !restricted,
            flag_settings,
        )
        .await;
        let mut d = plugin::discovery::discover_effective_plugins_with_bus(
            plugins_dir,
            &enabled,
            Some(analytics_bus),
        )
        .await;
        // Fallback: no allowlist match ⇒ flat-walk for direct plugin dirs.
        if d.is_empty() && ambient {
            d = plugin::discovery::discover_installed_plugins_with_bus(
                plugins_dir,
                Some(analytics_bus),
            )
            .await;
        }
        d
    } else {
        Vec::new()
    };
    if inline {
        discovered.extend(
            plugin::discovery::discover_cli_plugin_dirs_with_bus(
                cli_plugin_dirs,
                Some(analytics_bus),
            )
            .await,
        );
    }
    discovered
}

/// Component tallies reported by [`PluginRuntime::refresh`], mirroring
/// claude-code's `RefreshActivePluginsResult` (`utils/plugins/refresh.ts`). The
/// CLI formats these into the `/reload-plugins` confirmation line.
#[derive(Debug, Default, Clone, Copy)]
pub struct PluginRefreshCounts {
    /// Plugins now live in the session (successfully enabled).
    pub enabled: usize,
    /// Slash-commands (claude-code labels these "skills") across enabled plugins.
    pub commands: usize,
    /// Agents contributed by enabled plugins.
    pub agents: usize,
    /// Hook matchers across enabled plugins.
    pub hooks: usize,
    /// Plugin MCP servers across enabled plugins.
    pub mcp: usize,
    /// Plugin LSP servers across enabled plugins.
    pub lsp: usize,
    /// Plugins that failed to (re)load during the refresh.
    pub errors: usize,
}

/// Composition-root implementation of the orchestrator's runtime root-refresh
/// seam. It owns the live command registry and a late-bound plugin runtime
/// because plugins are materialised after the orchestrator is constructed.
struct DesktopRepoRootReloader {
    registry: Arc<RwLock<CommandRegistry>>,
    cwd: std::path::PathBuf,
    lingxi_home: std::path::PathBuf,
    managed_dir: Option<std::path::PathBuf>,
    home: std::path::PathBuf,
    safe_mode: bool,
    registered_roots: Arc<RwLock<Vec<std::path::PathBuf>>>,
    plugin_runtime: Arc<RwLock<Option<Arc<PluginRuntime>>>>,
}

impl DesktopRepoRootReloader {
    fn new(
        registry: Arc<RwLock<CommandRegistry>>,
        cwd: std::path::PathBuf,
        lingxi_home: std::path::PathBuf,
        safe_mode: bool,
    ) -> Self {
        Self {
            registry,
            cwd,
            lingxi_home,
            managed_dir: Some(crate::desktop::settings_watch::managed_settings_dir()),
            home: dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from(".")),
            safe_mode,
            registered_roots: Arc::new(RwLock::new(Vec::new())),
            plugin_runtime: Arc::new(RwLock::new(None)),
        }
    }

    fn registered_roots(&self) -> Arc<RwLock<Vec<std::path::PathBuf>>> {
        self.registered_roots.clone()
    }

    async fn set_plugin_runtime(&self, runtime: Option<Arc<PluginRuntime>>) {
        *self.plugin_runtime.write().await = runtime;
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::RepoRootReloader for DesktopRepoRootReloader {
    async fn reload(
        &self,
        request: lingxi_core::host::RepoRootReloadRequest,
    ) -> lingxi_core::host::RepoRootReloadOutcome {
        {
            let mut roots = self.registered_roots.write().await;
            if !roots.contains(&request.root) {
                roots.push(request.root.clone());
            }
        }

        let mut outcome = lingxi_core::host::RepoRootReloadOutcome::default();
        if request.reload_skills {
            let roots = self.registered_roots.read().await.clone();
            let additional_skill_dirs = roots
                .iter()
                .map(|root| root.join(branding::DOT_DIR).join("skills"))
                .collect();
            let handler = command_api::builtins::ReloadSkillsHandler::with_all_roots(
                self.registry.clone(),
                self.cwd.clone(),
                self.lingxi_home.clone(),
                self.managed_dir.clone(),
                self.home.clone(),
                additional_skill_dirs,
                self.safe_mode,
            );
            match parse_slash_command("/reload-skills") {
                Some(parsed) => match handler.handle(&parsed).await {
                    CommandResult::Done { .. } => outcome.skills_reloaded = true,
                    _ => outcome
                        .errors
                        .push("skill catalog returned a non-terminal reload result".to_string()),
                },
                None => outcome
                    .errors
                    .push("internal /reload-skills command parse failed".to_string()),
            }
        }

        if request.reload_plugins {
            let runtime = self.plugin_runtime.read().await.clone();
            match runtime {
                Some(runtime) => {
                    let counts = runtime.refresh().await;
                    outcome.plugins_reloaded = true;
                    if counts.errors > 0 {
                        outcome.errors.push(format!(
                            "plugin catalog reloaded with {} component error(s)",
                            counts.errors
                        ));
                    }
                }
                None => outcome
                    .errors
                    .push("plugin catalog is disabled in this runtime".to_string()),
            }
        }
        outcome
    }
}

/// (`/reload-plugins`) The live plugin subsystem, retained past startup so the
/// interactive `/reload-plugins` command can apply pending enable/disable
/// changes to the RUNNING session without a restart — claude-code's
/// `refreshActivePlugins` (Layer-3 refresh). Holds the SAME [`plugin::PluginManager`]
/// the startup bootstrap materialised through (its registries are the shared
/// `Arc`s the orchestrator reads), plus the discovery ingredients, so `refresh`
/// re-reads `enabledPlugins` off disk and diffs it against what is loaded:
/// `disable()` for plugins turned off (drops their commands/hooks/MCP/LSP from
/// the live registries), and only if every unload succeeds `enable()` for
/// newly-on ones (re-materialises + live-dials MCP), plus a wholesale rebuild
/// of the plugin-agent catalog portion.
pub struct PluginRuntime {
    manager: Arc<plugin::PluginManager>,
    analytics_bus: Arc<telemetry::AnalyticsBus>,
    plugins_dir: std::path::PathBuf,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
    cli_plugin_dirs: Vec<std::path::PathBuf>,
    additional_project_roots: Arc<RwLock<Vec<std::path::PathBuf>>>,
    ambient: bool,
    inline: bool,
    /// Session settings provenance. Restricted refreshes must not re-open
    /// ambient user/project/local plugin configuration.
    restricted: bool,
    flag_settings: Option<lingxi_core::settings::SettingsJson>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PluginRuntime {
    /// Re-read the on-disk enabled set and reconcile it into the live session.
    /// Returns the component tallies for the confirmation message. Best-effort
    /// on load failures, but conservative on unload failures: if any currently
    /// loaded plugin fails to disable, the refresh stops before enabling fresh
    /// targets so ownership does not overlap or partially duplicate.
    pub async fn refresh(&self) -> PluginRefreshCounts {
        let _refresh_guard = self.refresh_lock.lock().await;
        self.manager
            .replace_plugin_configs(
                load_plugin_configs(&self.home, self.restricted, self.flag_settings.as_ref()).await,
            )
            .await;
        self.manager
            .replace_blocked_marketplaces(load_blocked_marketplaces().await)
            .await;
        self.manager
            .replace_managed_plugin_names(load_managed_plugin_names().await)
            .await;
        // (1) The fresh target set from disk + settings.
        let additional_project_roots = self.additional_project_roots.read().await.clone();
        let target = discover_plugin_set(
            self.ambient,
            self.inline,
            &self.home,
            &self.cwd,
            &self.plugins_dir,
            &self.cli_plugin_dirs,
            &additional_project_roots,
            self.restricted,
            self.flag_settings.as_ref(),
            &self.analytics_bus,
        )
        .await;

        // (2) Unload every currently-loaded plugin. A reload re-reads the WHOLE
        //     enabled set from disk (claude-code `clearAllCaches` +
        //     `loadAllPlugins`), and `discover_plugin_set` mints a fresh
        //     `PluginId` per discovery (`load_plugin_from_path`), so the reloaded
        //     set never aliases the old ids — disabling all here, then enabling
        //     the fresh target below, is the full swap. This also picks up
        //     edited-in-place plugin files, matching cc's full reload.
        let mut counts = PluginRefreshCounts::default();
        for id in self.manager.loaded_plugin_ids().await {
            if let Err(error) = self.manager.disable(&id).await {
                counts.errors += 1;
                tracing::warn!(
                    error = %error,
                    plugin_id = %id,
                    "/reload-plugins: plugin failed to disable; aborting enable phase"
                );
            }
        }
        if counts.errors > 0 {
            return counts;
        }

        // (3) Enable each target plugin. The manager validates agent privileges
        //     before materialising every component into the shared registries.
        for (id, manifest, dir) in target {
            // Tally BEFORE `manifest` moves into `enable`.
            let c = &manifest.components;
            let this = (
                c.commands.len() + c.skills.len(),
                c.agents.len(),
                c.hooks.len(),
                c.mcp_servers.len(),
                c.lsp_servers.len(),
            );
            match self.manager.enable(&id, manifest, dir).await {
                Ok(()) => {
                    counts.enabled += 1;
                    counts.commands += this.0;
                    counts.agents += this.1;
                    counts.hooks += this.2;
                    counts.mcp += this.3;
                    counts.lsp += this.4;
                }
                Err(e) => {
                    counts.errors += 1;
                    tracing::warn!(error = %e, "/reload-plugins: plugin failed to load");
                }
            }
        }
        counts
    }
}

/// Oracle `W$s`: collection watch is on unless
/// `CLAUDE_CODE_PLUGIN_DIR_WATCH=false`.
fn plugin_dir_watch_enabled() -> bool {
    match std::env::var("CLAUDE_CODE_PLUGIN_DIR_WATCH") {
        Ok(value) if value == "0" || value.eq_ignore_ascii_case("false") => false,
        _ => true,
    }
}

async fn cli_plugin_dir_collection_snapshot(
    paths: &[std::path::PathBuf],
) -> Vec<(std::path::PathBuf, Vec<String>)> {
    let mut out = Vec::new();
    for path in paths {
        let Ok(canonical) = tokio::fs::canonicalize(path).await else {
            continue;
        };
        if let Some(children) = plugin::cli_plugin_dir_collection_children(&canonical).await {
            out.push((canonical, children));
        }
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

/// Oracle `watchCollections`: when `--plugin-dir` is a folder of plugins,
/// children added or removed while running reload the live plugin set.
fn spawn_cli_plugin_dir_collection_watch(runtime: Arc<PluginRuntime>) {
    tokio::spawn(async move {
        let mut snapshot = cli_plugin_dir_collection_snapshot(&runtime.cli_plugin_dirs).await;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let next = cli_plugin_dir_collection_snapshot(&runtime.cli_plugin_dirs).await;
            if next != snapshot {
                snapshot = next;
                let _ = runtime.refresh().await;
            }
        }
    });
}

/// SKILLLIST.1: `CommandRegistry`-backed skill-listing provider for the per-turn
/// `skill_listing` system-reminder. Reads the shared registry lazily at turn time
/// and applies the TS `getSkillToolCommands` eligibility filter
/// (`commands.ts:565-583`): model-invocable prompt skills, excluding builtins,
/// keeping bundled/skills/deprecated-dir entries plus any with a user-specified
/// description or `whenToUse`.
/// Collects the `system_message` of each completed background (`async`) hook so
/// the orchestrator's per-turn `async_hook_response` reminder can fold them into
/// the next turn (claude-code `getAsyncHookResponseAttachments`). CONSUME-ONCE:
/// [`AsyncHookResponseProvider::take_pending_responses`] drains the buffer
/// (mirrors TS `removeDeliveredAsyncHooks`). A plain `std::sync::Mutex` — every
/// critical section is a brief push / `mem::take`, never held across an `await`.
#[derive(Clone, Default)]
struct AsyncHookResponseBuffer {
    responses: Arc<std::sync::Mutex<Vec<String>>>,
    rewake_target: Arc<std::sync::OnceLock<std::sync::Weak<dyn OrchestratorHandle>>>,
}

impl AsyncHookResponseBuffer {
    fn push(&self, text: String) {
        if let Ok(mut v) = self.responses.lock() {
            v.push(text);
        }
    }

    fn attach_rewake_target(&self, orchestrator: &Arc<ConversationOrchestrator>) {
        let target: Arc<dyn OrchestratorHandle> = orchestrator.clone();
        let _ = self.rewake_target.set(Arc::downgrade(&target));
    }

    async fn rewake(&self) {
        let Some(target) = self.rewake_target.get().and_then(std::sync::Weak::upgrade) else {
            return;
        };
        if let Err(error) = target.run_async_hook_rewake().await {
            tracing::warn!(error = %error, "async hook re-wake turn failed");
        }
    }
}

#[async_trait::async_trait]
impl orchestrator::prompt::async_hook_response::AsyncHookResponseProvider
    for AsyncHookResponseBuffer
{
    async fn take_pending_responses(&self) -> Vec<String> {
        self.responses
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    }
}

fn registry_skill_listing_provider(
    registry: Arc<RwLock<CommandRegistry>>,
    read_file_state: tool_api::read_file_state::ReadFileStateMap,
) -> Arc<dyn orchestrator::prompt::skill_listing::SkillListingProvider> {
    // Conditional-skill activation is SESSION state, not per-listing: once a
    // touched file has revealed a skill, a later turn whose touched set no
    // longer names that file must not hide it again.
    let conditional = Arc::new(std::sync::Mutex::new(skill_api::ConditionalSkills::new()));
    Arc::new(
        orchestrator::prompt::skill_listing::LazySkillListingProvider::new(move || {
            let registry = registry.clone();
            let conditional = conditional.clone();
            let read_file_state = read_file_state.clone();
            async move {
                use command_api::{CommandSource, SlashCommandKind};
                let reg = registry.read().await;
                // The read-state map is an LRU with a 100-entry cap, so a
                // touched path can be evicted. That is exactly why activation is
                // remembered rather than recomputed: a skill the model has been
                // shown must not vanish because its file aged out.
                let touched = read_file_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .keys();
                let root =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                reg.model_invocable_commands() // !disable_model_invocation (registry.rs)
                    .into_iter()
                    // TS `cmd.type === 'prompt'` — markdown/plugin commands,
                    // bundled commands, not builtin/mcp.
                    .filter(|c| {
                        matches!(
                            c.kind,
                            SlashCommandKind::Markdown { .. }
                                | SlashCommandKind::Plugin { .. }
                                // Bundled programmatic skills (`/loop`) are model-invocable.
                                | SlashCommandKind::Bundled { .. }
                        )
                    })
                    // TS `cmd.source !== 'builtin'`.
                    .filter(|c| c.source != CommandSource::Builtin)
                    // A CONDITIONAL skill (`paths:`) stays out of the listing
                    // until the session has touched a matching file (claude-code
                    // `lhr`). `read_file_state` is this port's record of what the
                    // session touched.
                    .filter(|c| match c.paths.as_deref() {
                        None => true,
                        Some(patterns) => conditional
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .is_available_named(&c.name, patterns, &touched, &root),
                    })
                    // TS loadedFrom ∈ {bundled,skills,commands_DEPRECATED} ||
                    //    hasUserSpecifiedDescription || whenToUse.
                    .filter(|c| {
                        matches!(
                            c.loaded_from.as_deref(),
                            Some("bundled" | "skills" | "commands_DEPRECATED")
                        ) || c.has_user_specified_description
                            || c.when_to_use.is_some()
                    })
                    .map(|c| orchestrator::prompt::skill_listing::SkillListingEntry {
                        name: c.name.clone(),
                        description: c.description.clone(),
                        when_to_use: c.when_to_use.clone(),
                        // TS `cmd.source === 'bundled'` (prompt.ts) — bundled
                        // skills are never truncated; mirror via loadedFrom.
                        is_bundled: c.loaded_from.as_deref() == Some("bundled"),
                    })
                    .collect()
            }
        }),
    )
}

/// Production [`mcp::oauth::OnAuthorizationUrl`] callback for OAuth-configured
/// remote MCP servers. The MCP OAuth flow ([`mcp::McpRegistry::connect`]) fires
/// this once per interactive flow with the authorization URL the user must visit
/// to grant consent.
///
/// There is no browser-open util in this workspace, so this is best-effort:
/// 1. Log the URL prominently at `info` level (the engine's only surfacing path
///    from this depth — the TUI/transport tails the tracing stream).
/// 2. Attempt a detached OS-native browser open (`open` on macOS, `xdg-open` on
///    Linux, `cmd /c start` on Windows), ignoring any failure.
///
/// Non-panicking and non-blocking: a failed spawn leaves the logged URL as the
/// fallback the user can copy by hand.
fn mcp_on_authorization_url() -> mcp::oauth::OnAuthorizationUrl {
    Arc::new(|url: &str| {
        tracing::info!(
            target: "lingxi::mcp::oauth",
            authorization_url = %url,
            "MCP OAuth: open this URL in a browser to authorize the server:\n  {url}",

        );
        // Best-effort detached browser open; failures are intentionally ignored.
        #[cfg(target_os = "macos")]
        let cmd: Option<(&str, &[&str])> = Some(("open", &[]));
        #[cfg(target_os = "linux")]
        let cmd: Option<(&str, &[&str])> = Some(("xdg-open", &[]));
        #[cfg(target_os = "windows")]
        let cmd: Option<(&str, &[&str])> = Some(("cmd", &["/c", "start", ""]));
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        let cmd: Option<(&str, &[&str])> = None;

        if let Some((program, prefix)) = cmd {
            let _ = std::process::Command::new(program)
                .args(prefix)
                .arg(url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    })
}

/// Per-user Claude temp-dir name — port of claude-code `getClaudeTempDirName`
/// (`permissions/filesystem.ts:307-315`): `claude-<uid>` on Unix (the uid keeps
/// per-user dirs apart in a shared `/tmp`). Shares the crate's single
/// [`current_uid`] helper (a SAFE `nix::unistd::getuid` wrapper) rather than a
/// second `getuid` crate, so the sandbox seed and the task-spool dir agree.
fn lingxi_temp_dir_name() -> String {
    format!("claude-{}", current_uid())
}

/// Base Claude temp dir for the task spool — port of `getClaudeTempDir`
/// (`permissions/filesystem.ts:331-346`): `$LINGXI_TMPDIR || /tmp`, joined
/// with [`lingxi_temp_dir_name`]. (claude resolves symlinks; the spool path only
/// needs to be writable + session-unique, so the realpath step is omitted.)
/// Distinct from the sandbox-seed [`lingxi_temp_dir`] (which returns the
/// realpath-resolved, trailing-separator String form).
fn lingxi_temp_dir_path() -> std::path::PathBuf {
    let base = std::env::var_os("LINGXI_TMPDIR").map_or_else(
        || std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from,
    );
    base.join(lingxi_temp_dir_name())
}

/// Sanitize a path string for use as a single dir component — port of
/// `sanitizePath` (`sessionStoragePortable.ts:311-319`): every non-alphanumeric
/// char becomes `-`. (The >255-char hash-suffix branch is omitted; project
/// paths in practice stay well under it.)
fn sanitize_path_component(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Session-scoped task-output dir — port of `getTaskOutputDir`
/// (`diskOutput.ts:50-55`): `<projectTempDir>/<sessionId>/tasks`, where
/// `projectTempDir = <claudeTempDir>/<sanitized-cwd>` (`getProjectTempDir`,
/// `permissions/filesystem.ts:376-378`).
///
/// Session-scoping (vs the old in-repo `<cwd>/.lingxi/tasks-output`) keeps
/// concurrent sessions in one project from clobbering each other's spools and
/// stops task output from polluting the working tree / git status. Rooting under
/// the project temp dir also makes reads auto-allowed by claude's
/// `checkReadableInternalPath`.
#[must_use]
pub fn session_task_output_dir(cwd: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    lingxi_core::host::task_output::session_output_dir(&lingxi_temp_dir_path(), cwd, session_id)
}

fn session_kind_for_job_tmp() -> Option<String> {
    std::env::var("LINGXI_SESSION_KIND")
        .or_else(|_| std::env::var("CLAUDE_CODE_SESSION_KIND"))
        .ok()
}

fn job_dir_from_env() -> Option<std::path::PathBuf> {
    std::env::var_os("LINGXI_JOB_DIR")
        .or_else(|| std::env::var_os("CLAUDE_JOB_DIR"))
        .map(std::path::PathBuf::from)
}

/// Session-scoped read-block allowances (oracle `XY` / `BK`): tool results,
/// project temp, and — for a `bg` job — `{jobDir}/tmp`.
#[must_use]
pub fn session_read_allowances_for_boot(
    lingxi_home: &std::path::Path,
    cwd: &std::path::Path,
    session_uuid: &str,
    session_kind: Option<&str>,
    job_dir: Option<&std::path::Path>,
) -> Vec<permission::SessionReadAllowance> {
    let mut allowances = vec![
        permission::SessionReadAllowance::directory(
            session::jsonl::tool_results_dir(lingxi_home, &cwd.to_string_lossy(), session_uuid),
            permission::TOOL_RESULT_READ_ALLOW_REASON,
        ),
        permission::SessionReadAllowance::directory(
            session_task_output_dir(cwd, session_uuid),
            permission::PROJECT_TEMP_READ_ALLOW_REASON,
        ),
    ];
    if let Some(job_tmp) = permission::job_tmp_session_allowance(lingxi_home, session_kind, job_dir)
    {
        allowances.push(job_tmp);
    }
    allowances
}

/// # Errors
///
/// Returns [`BuildError`] if the api-client or orchestrator cannot be
/// constructed (effectively infallible in the current wiring).
#[allow(clippy::too_many_lines)]
/// Forwards `llm-runtime`'s synchronous retry-status reports to the async
/// session [`OutputStream`] so the TUI can render "Retrying in Ns… (attempt
/// X/Y)" during a backoff. `report` runs inside the retry loop's tokio context,
/// so it spawns the async emit (fire-and-forget; retries are seconds apart).
struct OutputRetryReporter {
    output: Arc<dyn OutputStream>,
}

impl llm_runtime::RetryReporter for OutputRetryReporter {
    fn report(&self, info: llm_runtime::RetryInfo) {
        let output = self.output.clone();
        tokio::spawn(async move {
            output
                .emit_api_retry(&info.message, info.attempt, info.max_retries, info.delay_ms)
                .await;
        });
    }
}

/// (worktree-tmux-launch plan, Task 3) Apply `-w`/`--worktree [name]`'s boot
/// launch onto an already-constructed `ctx`: create a git worktree and swap
/// the session into it. Called exactly once, from [`build`] right after its
/// `tool_ctx` literal is complete (so `ctx.session_cwd` /
/// `ctx.worktree_session` / `ctx.worktree` are all wired) — extracted into its
/// own function so this exact sequence is unit-testable against a
/// `BuiltinToolContext` fixture without driving a full `build()`.
///
/// Mirrors `EnterWorktreeTool::call_create`'s create → swap → record sequence
/// (`tools/worktree/src/worktree.rs`) exactly, reusing its random-slug helper
/// (`gen_random_slug`, made `pub` for this) instead of duplicating it;
/// `create_worktree` itself runs the same `validate_worktree_slug` pre-flight
/// the tool path runs, so an invalid `--worktree <name>` surfaces the same
/// `WorktreeError::InvalidSlug` message, wrapped in [`BuildError::WorktreeLaunch`].
/// Unlike the tool (which can refuse "already in a worktree" / subagent-cwd-
/// override calls), boot starts from a fresh session with no prior worktree
/// and no subagent cwd override, so those tool-only guards do not apply here.
///
/// INERT INVARIANT: `worktree_launch == None` (the default, and every host
/// but a CLI session with `-w`/`--worktree` set) is a complete no-op — no
/// create, no swap, `ctx.worktree_session` untouched. `tmux_launch == None`
/// (the default) is independently inert — no tmux session is created and
/// `tmux_session_name` stays `None` — even when `worktree_launch` is `Some`.
///
/// (worktree-tmux-launch plan, Task 4) `tmux_launch: Some(_)` requires
/// `worktree_launch: Some(_)` (mirrors the CLI's own `--tmux` doc: "Create a
/// tmux session for the worktree (requires --worktree)"); `Some` tmux with
/// `None` worktree is a hard boot failure
/// ([`BuildError::TmuxRequiresWorktree`]), not a silent ignore. When both are
/// `Some`, AFTER the worktree above is created + swapped + recorded, this
/// derives the session name ([`platform_posix::worktree_tmux::worktree_tmux_session_name`],
/// keyed on the PRE-swap `original_cwd` as the repo root) and creates a
/// detached tmux session for it
/// ([`platform_posix::worktree_tmux::create_worktree_tmux_session`]) through
/// `ctx.process`/`ctx.sandbox`. A tmux failure is logged
/// (`tracing::warn!`) and does NOT fail boot — the worktree launch itself
/// already succeeded, and `WorktreeSession.tmux_session_name` simply stays
/// `None` — only a tmux SUCCESS writes the name into the shared
/// `ctx.worktree_session` cell.
async fn apply_worktree_launch(
    worktree_launch: &Option<String>,
    tmux_launch: &Option<String>,
    ctx: &BuiltinToolContext,
) -> Result<(), BuildError> {
    let Some(name_or_empty) = worktree_launch else {
        if tmux_launch.is_some() {
            return Err(BuildError::TmuxRequiresWorktree);
        }
        return Ok(());
    };
    let slug = if name_or_empty.is_empty() {
        tool_worktree::worktree::gen_random_slug()
    } else {
        name_or_empty.clone()
    };

    // Native-mode (`--tmux` with NO explicit value → `Some("")`) pre-flight,
    // byte-faithful to 206's `re = Dor() && a.tmux===!0` branch (@230041975):
    // bare `--tmux` hard-checks not-Windows + tmux-installed BEFORE creating
    // the worktree. `--tmux=classic` (any explicit value) is NOT native and
    // skips these — a missing tmux then degrades to the non-fatal
    // create-session warning below. (`--tmux requires --worktree` is enforced
    // for BOTH modes by the `worktree_launch == None` guard above; that is a
    // deliberate, safe superset of 206, which only checks it for native.)
    if tmux_launch.as_deref() == Some("") {
        if cfg!(windows) {
            return Err(BuildError::TmuxNotSupportedOnWindows);
        }
        if !platform_posix::worktree_tmux::tmux_is_installed(
            ctx.process.as_ref(),
            ctx.sandbox.as_ref(),
        )
        .await
        {
            return Err(BuildError::TmuxNotInstalled(
                platform_posix::worktree_tmux::tmux_install_hint().to_string(),
            ));
        }
    }

    // Captured BEFORE the swap below — the pre-launch boot cwd, which
    // `ExitWorktree` later restores (same contract as
    // `EnterWorktreeTool::record_worktree_session`), and (Task 4) the repo
    // root the tmux session name is derived from.
    let original_cwd = ctx.session_cwd.cwd();
    let handle = ctx
        .worktree
        .create_worktree(&slug, None, &[])
        .await
        .map_err(|e| BuildError::WorktreeLaunch(e.to_string()))?;
    let worktree_path = handle.path.clone();
    ctx.session_cwd
        .swap(handle.path.clone(), vec![handle.path.clone()]);
    *ctx.worktree_session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tool_api::WorktreeSession {
        original_cwd: original_cwd.clone(),
        worktree_path: handle.path,
        branch_name: handle.branch_name,
        base_commit: handle.base_commit,
        // This session CREATED the worktree (never entered a pre-existing
        // one), so `ExitWorktree` may remove it — same as
        // `EnterWorktreeTool::call_create`'s `entered_existing: false`.
        entered_existing: false,
        // Populated below (Task 4) when `tmux_launch.is_some()` AND the tmux
        // session actually gets created; `None` otherwise.
        tmux_session_name: None,
    });

    if tmux_launch.is_some() {
        let session_name =
            platform_posix::worktree_tmux::worktree_tmux_session_name(&original_cwd, &slug);
        match platform_posix::worktree_tmux::create_worktree_tmux_session(
            ctx.process.as_ref(),
            ctx.sandbox.as_ref(),
            &session_name,
            &worktree_path,
        )
        .await
        {
            Ok(()) => {
                // 206's CLI worktree-launch prints the session name + attach
                // hint on success (@225872424, `console.log("Created tmux
                // session: {S}\nTo attach: tmux attach -t {S}")`) so the user
                // can find it — without this the derived name is invisible.
                // Emitted on STDERR (not 206's stdout) so it never pollutes
                // `--print`/stream-json stdout; this follows the port's boot-
                // notice precedent (the settings-warning `eprintln!` in
                // `build`). Colorization (206 `ht.green`) is dropped.
                eprintln!(
                    "Created tmux session: {session_name}\nTo attach: tmux attach -t {session_name}"
                );
                if let Some(session) = ctx
                    .worktree_session
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    session.tmux_session_name = Some(session_name);
                }
            }
            Err(e) => {
                // Non-fatal: the worktree itself was already created+entered
                // above, so a tmux hiccup must not fail boot — it only means
                // `WorktreeSession.tmux_session_name` stays `None`. 206 also
                // surfaces this to the user (@225872... `console.error("Warning:
                // Failed to create tmux session: {error}")`), so print it (on
                // stderr, matching 206's `console.error`) in addition to the
                // structured `tracing::warn!`.
                eprintln!("Warning: Failed to create tmux session: {e}");
                tracing::warn!(
                    error = %e,
                    session_name = %session_name,
                    "--tmux: failed to create the worktree tmux session; continuing without it"
                );
            }
        }
    }

    Ok(())
}

/// The write half of this key pair lives in the CLI (`session_cost`), so the
/// round trip crosses a crate boundary with nothing but these two field names
/// and the `SessionId` spelling holding it together.
#[cfg(test)]
#[path = "tests/legacy_opening_balance_test.rs"]
mod legacy_opening_balance_test;

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/connected_fallback_tests.rs"]
mod connected_fallback_tests;

#[cfg(test)]
#[path = "tests/workspace_lease_forwarding_tests.rs"]
mod workspace_lease_forwarding_tests;

/// Round-5 review item 11's class member (1), handed to the gate by that
/// fixer's `needs_other_file`: the connect LOOP inside
/// [`build_agent_mcp_tool_set`] is one await EARLIER than the
/// `pool.allocate` window `agent::handle::McpCleanupGuard` now covers, and
/// its half-built `cleanups` vec had no owner at all.
#[cfg(test)]
#[path = "tests/desktop_agent_mcp_cleanup_guard_tests.rs"]
mod desktop_agent_mcp_cleanup_guard_tests;

/// Platform supervisor selected by the desktop composition root. Hosts use
/// this alias so bridge startup does not need another platform dependency.
#[cfg(unix)]
pub use platform_posix::process::supervisor as shell_supervisor;
#[cfg(windows)]
pub use platform_windows::process::supervisor as shell_supervisor;

#[cfg(test)]
#[path = "tests/bash_edit_diff_wiring_tests.rs"]
mod bash_edit_diff_wiring_tests;

#[cfg(test)]
#[path = "tests/read_auto_allow_wiring_tests.rs"]
mod read_auto_allow_wiring_tests;

mod assembly;
mod configuration;
mod credentials;
mod fusion_services;
mod permission_config;
mod platform;
mod shutdown;

pub use assembly::build;
pub use assembly::build_with_credential_stack;
pub use assembly::build_with_host_automation;
use configuration::api_provider;
use configuration::is_env_truthy;
pub use configuration::model_deprecation_warning;
use configuration::resolve_memory_feature_gates;
use configuration::ApiProvider;
pub use configuration::CustomizationGates;
pub use configuration::DesktopConfig;
pub use configuration::DesktopEngineConfig;
pub use configuration::DesktopSessionComposition;
pub use credentials::api_service_from_stack;
use credentials::aws_auth_refresher;
pub use credentials::build_api_service;
pub use credentials::build_shared_credential_stack;
use credentials::build_shared_credential_stack_for_config;
pub use credentials::build_shared_credential_stack_with_policy;
use credentials::capture_legacy_opening_balance;
use credentials::managed_settings_raw_tiers_sync;
pub use credentials::resolve_llm_stack;
use credentials::resolve_llm_stack_with_credentials;
use credentials::CredentialStoreAuthProvider;
pub use credentials::LlmStack;
pub use credentials::SharedCredentialStack;
use fusion_services::desktop_fusion_attempts;
use fusion_services::desktop_fusion_catalog_row;
use fusion_services::desktop_fusion_runtime_config;
use fusion_services::filter_fusion_catalog;
pub use fusion_services::fusion_credential_restart_required_message;
use fusion_services::fusion_route_flag;
pub use fusion_services::publish_fusion_catalog_credential;
pub use fusion_services::refresh_fusion_catalog_after_credential_delete;
pub use fusion_services::refresh_fusion_catalog_after_credential_write;
pub use fusion_services::register_fusion_catalog_refresher;
pub use fusion_services::spawn_fusion_catalog_refresh;
use fusion_services::DesktopFusionConfigSource;
use fusion_services::DesktopFusionExecutor;
use fusion_services::DesktopFusionPriceBook;
use fusion_services::FusionCatalogClearingAuth;
use fusion_services::FusionCatalogModelSource;
pub use fusion_services::FusionCatalogRefresher;
use fusion_services::FusionCatalogRefreshingChatGptConnect;
use fusion_services::FusionCatalogRefreshingCopilotConnect;
use fusion_services::FusionCatalogRefreshingCredentialWriter;
use fusion_services::FusionCatalogRefreshingOAuthConnect;
pub use fusion_services::FusionCatalogRegistry;
use permission_config::append_mcp_permission_rules;
use permission_config::append_restricted_builtin_denies;
use permission_config::apple_events_override;
use permission_config::cron_scheduler_enabled;
#[cfg(unix)]
use permission_config::current_uid;
#[cfg(not(unix))]
use permission_config::current_uid;
use permission_config::expand_trusted_dir;
use permission_config::lingxi_temp_dir;
use permission_config::load_boot_permission_tiers_with_flag;
pub use permission_config::managed_force_login_org_pin;
pub use permission_config::managed_model_allowlist;
use permission_config::managed_model_policy_source;
use permission_config::managed_model_setting_for_config;
use permission_config::managed_only_sandbox_overrides;
pub use permission_config::managed_otel_env_overrides;
use permission_config::model_provenance_for_config;
pub use permission_config::platform_in_enabled_list;
use permission_config::ripgrep_override;
use permission_config::sandbox_auto_allow_from_settings_tiers;
use permission_config::sandbox_runtime_config_from_settings_tiers;
use permission_config::should_enforce_permissions;
use permission_config::strict_allowlist_override;
use permission_config::BootPermissionTiers;
pub use shutdown::refresh_process_session_presence;
#[cfg(any(unix, windows))]
pub use shutdown::supervisor_exit_sink;
pub use shutdown::DesktopSessionLifecycle;
pub use shutdown::DesktopSessionShutdownReport;
use shutdown::ProcessSessionActivationObserver;
pub use shutdown::DESKTOP_SHUTDOWN_BUDGET;

#[cfg(test)]
use assembly::resolve_workspace_trust;
#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use fusion_services::publish_fusion_catalog_credential_to;
#[cfg(test)]
use orchestrator::{QUERY_SOURCE_REPL_MAIN_THREAD, QUERY_SOURCE_SDK};
#[cfg(test)]
use permission_config::fold_managed_otel_env_overrides;

#[cfg(test)]
use permission_config::load_boot_permission_tiers;
