//! Mock fixtures for M4-05 wiring tests.
//!
//! Recording mocks for the 4 handle traits the M4-05 tools dispatch
//! through. The Arc::ptr_eq recursion-lock + budget-inheritance tests
//! work by capturing what the production `AgentTool::call` hands to the
//! spawner and asserting the trait-object Arcs match the originals.

use async_trait::async_trait;
use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
use lingxi_core::host::mailbox::{MailboxError, MailboxMessage, MailboxRouterHandle, RouteAck};
use lingxi_core::host::subagent_spawn::{
    AgentSpawnAdmission, AgentSpawnStart, SubagentInheritance, SubagentListingEntry,
    SubagentResult, SubagentSpawnError, SubagentSpawnRequest, SubagentSpawner, SubagentUsage,
};
use lingxi_core::host::task_registry::{
    TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
    TaskRegistryHandle, TaskUpdatePatch,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

// =========================================================================
// MockSubagentSpawner — records every spawn + exposes captured inheritance.
// =========================================================================

/// One captured invocation of `MockSubagentSpawner::spawn`.
#[derive(Clone)]
pub struct MockSpawnInvocation {
    /// The request passed to spawn.
    pub request: SubagentSpawnRequest,
    /// The inheritance bundle passed to spawn. Captured verbatim — the
    /// `Arc` fields support `Arc::ptr_eq` round-trip against whatever the
    /// parent originally constructed.
    pub inherit: SubagentInheritance,
}

/// Recording mock for `SubagentSpawner`.
pub struct MockSubagentSpawner {
    invocations: Mutex<Vec<MockSpawnInvocation>>,
    teammate_enabled: Mutex<bool>,
    response: Mutex<MockSpawnResponse>,
    handback: Mutex<Option<lingxi_core::host::handback::HandbackState>>,
    /// `required_mcp_servers` surfaced from `resolve_required_mcp_servers` (the
    /// `#G3` pre-spawn MCP gate). Default empty (no requirement).
    required_mcp_servers: Mutex<Vec<String>>,
    /// Optional scripted `SelectedAgentMeta` for `resolve_selection` (G11 — the
    /// `tengu_agent_tool_selected` event). `None` ⇒ the default minimal meta.
    selection: Mutex<Option<lingxi_core::host::subagent_spawn::SelectedAgentMeta>>,
    /// Captured `register_name(name, agent_id)` calls (G14) so tests can assert
    /// that a name-carrying spawn registered the mapping.
    registered_names: Mutex<Vec<(String, lingxi_core::types::AgentId)>>,
    /// When `true`, `spawn_async` behaves as the DEFAULT unwired stub (returns a
    /// clear `Internal` error, no invocation recorded) so the "unwired async
    /// surfaces an error, not a silent sync fallback" invariant stays testable.
    /// Default `false` (wired — returns an `AsyncLaunch`).
    async_unwired: Mutex<bool>,
    /// Scripted active-subagent count for the 2.1.217 concurrency-cap gate.
    concurrent_subagents: AtomicUsize,
    /// Optional override for `agent_listing` — `None` ⇒ the default
    /// general-purpose / Explore / Plan catalog. Used by the 2.1.238
    /// `subagent_type is required` tests, which need a catalog WITHOUT
    /// `general-purpose`.
    listing_override: Mutex<Option<Vec<SubagentListingEntry>>>,
    /// Separate model-facing offer projection. `None` follows the raw listing.
    model_listing_override: Mutex<Option<Vec<SubagentListingEntry>>>,
    /// Scripted `tools_denied_agent_types` (claude 2.1.238 `NJa`/`mdr`,
    /// @290291941). Default empty ⇒ nothing withheld.
    tools_denied: Mutex<Vec<String>>,
    agent_spawn_rewrite: Mutex<Option<serde_json::Value>>,
    agent_spawn_answer: Mutex<Option<serde_json::Value>>,
    agent_spawn_started: Arc<Mutex<Vec<(lingxi_core::types::AgentId, String)>>>,
    agent_spawn_inputs: Mutex<Vec<serde_json::Value>>,
    agent_spawn_provenances: Mutex<Vec<lingxi_core::host::subagent_spawn::AgentSpawnProvenance>>,
}

struct MockAgentSpawnStart(Arc<Mutex<Vec<(lingxi_core::types::AgentId, String)>>>);

impl AgentSpawnStart for MockAgentSpawnStart {
    fn started(self: Box<Self>, agent_id: lingxi_core::types::AgentId, model: String) {
        self.0.lock().unwrap().push((agent_id, model));
    }

    fn failed(self: Box<Self>, _reason: String) {}
}

#[derive(Clone)]
enum MockSpawnResponse {
    Completed,
    /// A completed result with caller-supplied claude `content` (the runner's
    /// terminal result JSON), usage, and result-level totals — used by the `#3`
    /// return-shape / `model_content` golden tests.
    CompletedWith {
        agent_id: lingxi_core::types::AgentId,
        content: serde_json::Value,
        usage: SubagentUsage,
        total_tool_use_count: u64,
        total_duration_ms: u64,
        total_tokens: u64,
        assistant_message_count: u64,
        response_char_count: u64,
        last_request_id: Option<String>,
    },
    Failed(String),
    Killed,
    PoolFull,
}

impl MockSubagentSpawner {
    /// Build a mock that returns `SubagentResult::Completed`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            invocations: Mutex::new(Vec::new()),
            teammate_enabled: Mutex::new(false),
            response: Mutex::new(MockSpawnResponse::Completed),
            handback: Mutex::new(None),
            required_mcp_servers: Mutex::new(Vec::new()),
            selection: Mutex::new(None),
            registered_names: Mutex::new(Vec::new()),
            async_unwired: Mutex::new(false),
            concurrent_subagents: AtomicUsize::new(0),
            listing_override: Mutex::new(None),
            model_listing_override: Mutex::new(None),
            tools_denied: Mutex::new(Vec::new()),
            agent_spawn_rewrite: Mutex::new(None),
            agent_spawn_answer: Mutex::new(None),
            agent_spawn_started: Arc::new(Mutex::new(Vec::new())),
            agent_spawn_inputs: Mutex::new(Vec::new()),
            agent_spawn_provenances: Mutex::new(Vec::new()),
        }
    }

    /// Enable the implicit team in this mock session.
    pub fn enable_teammates(&self) {
        *self.teammate_enabled.lock().unwrap() = true;
    }

    /// Replace the catalog returned by `agent_listing`.
    pub fn set_agent_listing(&self, entries: Vec<SubagentListingEntry>) {
        *self.listing_override.lock().unwrap() = Some(entries);
    }

    /// Replace only the model-facing offer projection; explicit resolution
    /// continues to use `agent_listing`.
    pub fn set_agent_listing_for_model(&self, entries: Vec<SubagentListingEntry>) {
        *self.model_listing_override.lock().unwrap() = Some(entries);
    }

    /// Script the agent types whose every tool is denied (claude `NJa`).
    pub fn set_tools_denied_agent_types(&self, types: Vec<String>) {
        *self.tools_denied.lock().unwrap() = types;
    }

    /// Have the mock Mod bridge forward a rewritten spawn event.
    pub fn script_agent_spawn_rewrite(&self, rewrite: serde_json::Value) {
        *self.agent_spawn_rewrite.lock().unwrap() = Some(rewrite);
    }

    /// Have the mock Mod bridge answer without forwarding to a spawner.
    pub fn script_agent_spawn_answer(&self, answer: serde_json::Value) {
        *self.agent_spawn_answer.lock().unwrap() = Some(answer);
    }

    /// Receipts sent by AgentTool after a child was launched.
    pub fn agent_spawn_started(&self) -> Vec<(lingxi_core::types::AgentId, String)> {
        self.agent_spawn_started.lock().unwrap().clone()
    }

    /// Inputs presented by AgentTool before a scripted Mod rewrite.
    pub fn agent_spawn_inputs(&self) -> Vec<serde_json::Value> {
        self.agent_spawn_inputs.lock().unwrap().clone()
    }

    /// Provenance sidecars passed alongside each pre-spawn hook input.
    pub fn agent_spawn_provenances(
        &self,
    ) -> Vec<lingxi_core::host::subagent_spawn::AgentSpawnProvenance> {
        self.agent_spawn_provenances.lock().unwrap().clone()
    }

    /// Set the active-subagent count returned at the pre-spawn boundary.
    pub fn set_concurrent_subagents(&self, count: usize) {
        self.concurrent_subagents.store(count, Ordering::SeqCst);
    }

    /// Make `spawn_async` behave as the DEFAULT unwired stub (clear error, no
    /// invocation recorded) — for the "unwired async → error, not silent sync"
    /// test.
    pub fn set_async_unwired(&self) {
        *self.async_unwired.lock().unwrap() = true;
    }

    /// Script the `SelectedAgentMeta` the next `resolve_selection` returns (G11).
    pub fn script_selection(&self, meta: lingxi_core::host::subagent_spawn::SelectedAgentMeta) {
        *self.selection.lock().unwrap() = Some(meta);
    }

    /// Drain and return the captured `register_name` calls (G14).
    #[must_use]
    pub fn registered_names(&self) -> Vec<(String, lingxi_core::types::AgentId)> {
        self.registered_names.lock().unwrap().clone()
    }

    /// Script the `required_mcp_servers` the next `resolve_required_mcp_servers`
    /// returns (the `#G3` pre-spawn MCP gate).
    pub fn script_required_mcp_servers(&self, servers: Vec<String>) {
        *self.required_mcp_servers.lock().unwrap() = servers;
    }

    /// Force the next (and subsequent) spawns to fail.
    pub fn script_failed(&self, reason: impl Into<String>) {
        *self.response.lock().unwrap() = MockSpawnResponse::Failed(reason.into());
    }

    /// Attach typed reporting state to subsequent completed responses so tests
    /// exercise AgentTool's send, flagged, and withheld result projection.
    pub fn script_handback(&self, state: lingxi_core::host::handback::HandbackState) {
        *self.handback.lock().unwrap() = Some(state);
    }

    /// Script a fully-specified completed result (claude `content` JSON, usage,
    /// totals) so `AgentTool::call` builds its real result shape + `model_content`.
    #[allow(clippy::too_many_arguments)]
    pub fn script_completed_with(
        &self,
        agent_id: lingxi_core::types::AgentId,
        content: serde_json::Value,
        usage: SubagentUsage,
        total_tool_use_count: u64,
        total_duration_ms: u64,
        total_tokens: u64,
    ) {
        *self.response.lock().unwrap() = MockSpawnResponse::CompletedWith {
            agent_id,
            content,
            usage,
            total_tool_use_count,
            total_duration_ms,
            total_tokens,
            assistant_message_count: 0,
            response_char_count: 0,
            last_request_id: None,
        };
    }

    /// Script a completed result additionally carrying the G11 completed-event
    /// rollups (`assistant_message_count` / `response_char_count` /
    /// `last_request_id`) so the `tengu_agent_tool_completed` /
    /// `tengu_cache_eviction_hint` emit tests can drive them.
    #[allow(clippy::too_many_arguments)]
    pub fn script_completed_full(
        &self,
        agent_id: lingxi_core::types::AgentId,
        content: serde_json::Value,
        usage: SubagentUsage,
        total_tool_use_count: u64,
        total_duration_ms: u64,
        total_tokens: u64,
        assistant_message_count: u64,
        response_char_count: u64,
        last_request_id: Option<String>,
    ) {
        *self.response.lock().unwrap() = MockSpawnResponse::CompletedWith {
            agent_id,
            content,
            usage,
            total_tool_use_count,
            total_duration_ms,
            total_tokens,
            assistant_message_count,
            response_char_count,
            last_request_id,
        };
    }

    /// Force spawns to return Killed.
    pub fn script_killed(&self) {
        *self.response.lock().unwrap() = MockSpawnResponse::Killed;
    }

    /// Force allocation to lose a concurrent pool-cap race.
    pub fn script_pool_full(&self) {
        *self.response.lock().unwrap() = MockSpawnResponse::PoolFull;
    }

    /// Drain and return every captured invocation.
    #[must_use]
    pub fn invocations(&self) -> Vec<MockSpawnInvocation> {
        self.invocations.lock().unwrap().clone()
    }
}

impl Default for MockSubagentSpawner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SubagentSpawner for MockSubagentSpawner {
    async fn begin_agent_spawn(
        &self,
        mut input: serde_json::Value,
        provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    ) -> Result<AgentSpawnAdmission, SubagentSpawnError> {
        self.agent_spawn_inputs.lock().unwrap().push(input.clone());
        self.agent_spawn_provenances
            .lock()
            .unwrap()
            .push(provenance);
        if let Some(answer) = self.agent_spawn_answer.lock().unwrap().clone() {
            return Ok(AgentSpawnAdmission::Answered(answer));
        }
        let Some(rewrite) = self.agent_spawn_rewrite.lock().unwrap().clone() else {
            return Ok(AgentSpawnAdmission::Bypass(input));
        };
        for (key, value) in rewrite
            .as_object()
            .expect("scripted Mod rewrite is an object")
        {
            input[key] = value.clone();
        }
        Ok(AgentSpawnAdmission::Forwarded {
            input,
            start: Box::new(MockAgentSpawnStart(self.agent_spawn_started.clone())),
        })
    }

    fn teammate_enabled(&self) -> bool {
        *self.teammate_enabled.lock().unwrap()
    }

    async fn spawn_teammate(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<lingxi_core::host::team_spawn::TeammateLaunch, SubagentSpawnError> {
        self.invocations.lock().unwrap().push(MockSpawnInvocation {
            request: request.clone(),
            inherit,
        });
        Ok(lingxi_core::host::team_spawn::TeammateLaunch {
            teammate_id: "scout@session".into(),
            agent_id: lingxi_core::types::AgentId::new().to_string(),
            agent_type: request.subagent_type,
            model: request.model.unwrap_or_else(|| "sonnet".into()),
            model_profile: request.model_profile,
            name: request.name.unwrap(),
            color: "blue".into(),
            tmux_session_name: String::new(),
            tmux_window_name: String::new(),
            tmux_pane_id: "in-process".into(),
            team_name: "session".into(),
            is_splitpane: false,
            plan_mode_required: false,
        })
    }

    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.invocations.lock().unwrap().push(MockSpawnInvocation {
            request: request.clone(),
            inherit: inherit.clone(),
        });
        let resp = self.response.lock().unwrap().clone();
        Ok(match resp {
            // Mock: no real child exists, so a fresh AgentId is acceptable HERE.
            MockSpawnResponse::Completed => SubagentResult::Completed {
                handback: self.handback.lock().unwrap().clone(),
                agent_id: lingxi_core::types::AgentId::new(),
                content: json!({ "mock": true }),
                usage: SubagentUsage::default(),
                total_tool_use_count: 0,
                total_duration_ms: 0,
                total_tokens: 0,
                assistant_message_count: 0,
                response_char_count: 0,
                last_request_id: None,
                cumulative_usage: SubagentUsage::default(),
                usage_complete: true,
            },
            MockSpawnResponse::CompletedWith {
                agent_id,
                content,
                usage,
                total_tool_use_count,
                total_duration_ms,
                total_tokens,
                assistant_message_count,
                response_char_count,
                last_request_id,
            } => SubagentResult::Completed {
                handback: self.handback.lock().unwrap().clone(),
                agent_id,
                content,
                usage: usage.clone(),
                total_tool_use_count,
                total_duration_ms,
                total_tokens,
                assistant_message_count,
                response_char_count,
                last_request_id,
                cumulative_usage: usage,
                usage_complete: true,
            },
            MockSpawnResponse::Failed(reason) => SubagentResult::Failed {
                agent_id: lingxi_core::types::AgentId::new(),
                reason,
                usage: SubagentUsage::default(),
            },
            MockSpawnResponse::Killed => SubagentResult::Killed {
                agent_id: lingxi_core::types::AgentId::new(),
            },
            MockSpawnResponse::PoolFull => return Err(SubagentSpawnError::PoolFull),
        })
    }

    async fn concurrent_subagent_count(&self) -> usize {
        self.concurrent_subagents.load(Ordering::SeqCst)
    }

    /// Records the spawn request (so tests can assert the threaded
    /// `tool_use_id`) and returns a fixed [`lingxi_core::host::subagent_spawn::AsyncLaunch`]
    /// — overriding the defaulted "not wired" stub so the async dispatch path is
    /// exercisable in tests.
    async fn spawn_async(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<lingxi_core::host::subagent_spawn::AsyncLaunch, SubagentSpawnError> {
        if *self.async_unwired.lock().unwrap() {
            // Mirror the default trait stub — no invocation recorded (no silent
            // sync fallback).
            return Err(SubagentSpawnError::Internal(
                "async subagent spawn (run_in_background) is not wired in this build".to_string(),
            ));
        }
        if matches!(&*self.response.lock().unwrap(), MockSpawnResponse::PoolFull) {
            return Err(SubagentSpawnError::PoolFull);
        }
        self.invocations
            .lock()
            .unwrap()
            .push(MockSpawnInvocation { request, inherit });
        Ok(lingxi_core::host::subagent_spawn::AsyncLaunch {
            agent_id: lingxi_core::types::AgentId::new(),
            output_file: "/tmp/mock-agent.output".to_string(),
        })
    }

    /// Fixed catalog so the dynamic-prompt test can assert `formatAgentLine`
    /// output AND the `#1` explicit-unknown gate (`AgentTool::call` validates an
    /// explicit `subagent_type` against this listing). Mirrors the shape the
    /// production spawner surfaces (built-ins). `Plan` is included so the
    /// budget-gate test (which spawns `Plan`) clears the type validation that now
    /// precedes the budget gate.
    /// claude 2.1.238 `NJa` (@290291941) — scripted, default empty.
    async fn tools_denied_agent_types(&self) -> Vec<String> {
        self.tools_denied.lock().unwrap().clone()
    }

    async fn agent_listing(&self) -> Vec<SubagentListingEntry> {
        if let Some(entries) = self.listing_override.lock().unwrap().clone() {
            return entries;
        }
        vec![
            SubagentListingEntry {
                agent_type: "general-purpose".into(),
                when_to_use: "use for anything".into(),
                when_to_use_lean: None,
                tools_description: "All tools".into(),
            },
            SubagentListingEntry {
                agent_type: "Explore".into(),
                when_to_use: "search".into(),
                when_to_use_lean: None,
                tools_description: "All tools except Edit".into(),
            },
            SubagentListingEntry {
                agent_type: "Plan".into(),
                when_to_use: "plan a task".into(),
                when_to_use_lean: None,
                tools_description: "All tools except Edit".into(),
            },
        ]
    }

    async fn agent_listing_for_model(&self) -> Vec<SubagentListingEntry> {
        if let Some(entries) = self.model_listing_override.lock().unwrap().clone() {
            return entries;
        }
        self.agent_listing().await
    }

    /// Surface the scripted `required_mcp_servers` (default empty — built-ins
    /// declare none). Drives the `#G3` pre-spawn MCP gate in `AgentTool::call`.
    async fn resolve_required_mcp_servers(&self, _subagent_type: &str) -> Vec<String> {
        self.required_mcp_servers.lock().unwrap().clone()
    }

    /// Surface the scripted `SelectedAgentMeta` (G11). When none is scripted,
    /// echo the `subagent_type` (the trait default shape) so the
    /// `tengu_agent_tool_selected` emit still has an `agent_type`.
    async fn resolve_selection(
        &self,
        subagent_type: &str,
        _model: Option<&str>,
    ) -> lingxi_core::host::subagent_spawn::SelectedAgentMeta {
        self.selection.lock().unwrap().clone().unwrap_or(
            lingxi_core::host::subagent_spawn::SelectedAgentMeta {
                agent_type: subagent_type.to_string(),
                ..lingxi_core::host::subagent_spawn::SelectedAgentMeta::default()
            },
        )
    }

    /// Capture `register_name` calls (G14) so tests can assert the registration.
    async fn register_name(&self, name: &str, agent_id: lingxi_core::types::AgentId) {
        self.registered_names
            .lock()
            .unwrap()
            .push((name.to_string(), agent_id));
    }

    async fn resolve_name(&self, name: &str) -> Option<lingxi_core::types::AgentId> {
        self.registered_names
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id)
    }
}

// =========================================================================
// MockTaskRegistryHandle — in-memory CRUD.
// =========================================================================

/// In-memory recording mock for `TaskRegistryHandle`.
pub struct MockTaskRegistryHandle {
    resume_recipes: Mutex<HashMap<String, (SubagentSpawnRequest, SubagentInheritance)>>,
    reject_resume_recipe: std::sync::atomic::AtomicBool,
    killers: Mutex<HashMap<String, Arc<dyn lingxi_core::host::task_registry::TaskKiller>>>,
    receivers:
        Mutex<HashMap<String, Arc<dyn lingxi_core::host::task_registry::TaskMessageReceiver>>>,
    backgrounders:
        Mutex<HashMap<String, Arc<dyn lingxi_core::host::task_registry::TaskBackgrounder>>>,
    outcomes: Mutex<HashMap<String, lingxi_core::host::task_registry::AgentTerminalOutcome>>,
    records: Mutex<HashMap<String, TaskRecord>>,
    counter: AtomicU64,
    /// Per-session subagent-spawn counter backing `get_total_agent_spawns` /
    /// `increment_total_agent_spawns` (the 2.1.212 spawn-cap gate).
    spawns: AtomicU64,
}

impl MockTaskRegistryHandle {
    /// Exact foreground launch bundle saved before releasing the startup gate.
    pub fn resume_recipe(&self, id: &str) -> Option<(SubagentSpawnRequest, SubagentInheritance)> {
        self.resume_recipes.lock().unwrap().get(id).cloned()
    }
    /// Exercise a registry failure before the model is allowed to run.
    pub fn reject_resume_recipe_registration(&self) {
        self.reject_resume_recipe.store(true, Ordering::SeqCst);
    }

    /// Invoke the real foreground cancellation owner in tests.
    pub async fn kill_foreground_worker(&self, id: &str) {
        let killer = self.killers.lock().unwrap().get(id).cloned().unwrap();
        killer.kill().await;
    }

    /// Deliver to the actual externally bound foreground runner in tests.
    pub async fn send_foreground_message(
        &self,
        id: &str,
        message: String,
    ) -> Result<(), TaskRegistryError> {
        let receiver = self.receivers.lock().unwrap().get(id).cloned().unwrap();
        receiver.send(message).await
    }

    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            resume_recipes: Mutex::new(HashMap::new()),
            reject_resume_recipe: Default::default(),
            killers: Mutex::new(HashMap::new()),
            receivers: Mutex::new(HashMap::new()),
            backgrounders: Mutex::new(HashMap::new()),
            outcomes: Mutex::new(HashMap::new()),
            records: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            spawns: AtomicU64::new(0),
        }
    }

    /// Seed the per-session subagent-spawn counter, so a test can drive the
    /// `Agent` tool's spawn-cap gate deterministically.
    pub fn set_total_agent_spawns(&self, n: u64) {
        self.spawns.store(n, Ordering::SeqCst);
    }

    fn fresh_id(&self, task_type: &str) -> String {
        let prefix = match task_type {
            "local_agent" => 'a',
            "remote_agent" => 'r',
            "in_process_teammate" => 't',
            "local_workflow" => 'w',
            "monitor_mcp" => 'm',
            "dream" => 'd',
            "local_fusion" => 'f',
            // local_bash + any unknown wire string default to 'b'.
            _ => 'b',
        };
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        // 8-char base36 suffix, padded zero — deterministic for tests.
        format!("{prefix}{n:08x}")
            .chars()
            .take(9)
            .collect::<String>()
    }
}

impl Default for MockTaskRegistryHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskRegistryHandle for MockTaskRegistryHandle {
    async fn register_agent_resume_recipe(
        &self,
        id: &str,
        request: SubagentSpawnRequest,
        inheritance: SubagentInheritance,
    ) -> Result<(), TaskRegistryError> {
        if !self.records.lock().unwrap().contains_key(id) {
            return Err(TaskRegistryError::NotFound(id.into()));
        }
        if self.reject_resume_recipe.load(Ordering::SeqCst) {
            return Err(TaskRegistryError::Internal("resume recipe rejected".into()));
        }
        self.resume_recipes
            .lock()
            .unwrap()
            .insert(id.into(), (request, inheritance));
        Ok(())
    }
    async fn bind_agent_message_receiver(
        &self,
        id: &str,
        receiver: Arc<dyn lingxi_core::host::task_registry::TaskMessageReceiver>,
    ) -> Result<(), TaskRegistryError> {
        self.receivers
            .lock()
            .unwrap()
            .insert(id.to_string(), receiver);
        Ok(())
    }

    async fn register_foreground_agent(
        &self,
        registration: lingxi_core::host::task_registry::ForegroundAgentRegistration,
    ) -> Result<lingxi_core::host::task_registry::ForegroundAgentHandle, TaskRegistryError> {
        let id = self.fresh_id("local_agent");
        self.records.lock().unwrap().insert(
            id.clone(),
            TaskRecord {
                task_id: id.clone(),
                task_type: "local_agent".into(),
                status: "running".into(),
                owner_agent_id: Some(registration.agent_id.to_string()),
                is_backgrounded: Some(false),
                agent_facts: None,
                ..Default::default()
            },
        );
        Ok(lingxi_core::host::task_registry::ForegroundAgentHandle {
            task_id: id.clone(),
            output_path: format!("/tmp/{id}.output"),
        })
    }
    async fn unregister_foreground_agent(&self, id: &str) {
        let mut records = self.records.lock().unwrap();
        if records
            .get(id)
            .is_some_and(|r| r.is_backgrounded != Some(true))
        {
            records.remove(id);
        }
    }
    async fn bind_background_killer(
        &self,
        id: &str,
        killer: Arc<dyn lingxi_core::host::task_registry::TaskKiller>,
    ) -> Result<(), TaskRegistryError> {
        self.killers.lock().unwrap().insert(id.to_string(), killer);
        Ok(())
    }
    async fn bind_background_requester(
        &self,
        id: &str,
        requester: Arc<dyn lingxi_core::host::task_registry::TaskBackgrounder>,
    ) -> Result<(), TaskRegistryError> {
        self.backgrounders
            .lock()
            .unwrap()
            .insert(id.into(), requester);
        Ok(())
    }
    async fn background_task(&self, id: &str) -> bool {
        let requester = self.backgrounders.lock().unwrap().get(id).cloned();
        if let Some(requester) = requester {
            self.records
                .lock()
                .unwrap()
                .get_mut(id)
                .unwrap()
                .is_backgrounded = Some(true);
            requester.background().await;
            true
        } else {
            false
        }
    }
    async fn set_agent_outcome(
        &self,
        id: &str,
        outcome: lingxi_core::host::task_registry::AgentTerminalOutcome,
    ) {
        self.outcomes.lock().unwrap().insert(id.into(), outcome);
    }

    fn get_total_agent_spawns(&self) -> u64 {
        self.spawns.load(Ordering::SeqCst)
    }

    fn increment_total_agent_spawns(&self) {
        self.spawns.fetch_add(1, Ordering::SeqCst);
    }

    fn try_reserve_total_agent_spawn(&self, cap: u64) -> Result<u64, u64> {
        self.spawns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < cap).then(|| current + 1)
            })
            .map(|previous| previous + 1)
    }

    fn release_total_agent_spawn_reservation(&self) {
        self.release_total_agent_spawn_reservations(1);
    }

    fn try_reserve_total_agent_spawns(&self, n: u64, cap: u64) -> Result<u64, u64> {
        if n == 0 {
            return Ok(self.spawns.load(Ordering::SeqCst));
        }
        self.spawns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(n).filter(|&next| next <= cap)
            })
            .map(|previous| previous + n)
    }

    fn release_total_agent_spawn_reservations(&self, n: u64) {
        if n == 0 {
            return;
        }
        let _ = self
            .spawns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_sub(n))
            });
    }

    async fn create(&self, input: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
        let rec = TaskRecord {
            task_id: self.fresh_id(&input.task_type),
            task_type: input.task_type,
            status: "pending".into(),
            description: input.description,
            command: None,
            agent_facts: None,
            ..Default::default()
        };
        self.records
            .lock()
            .unwrap()
            .insert(rec.task_id.clone(), rec.clone());
        Ok(rec)
    }

    async fn get(&self, id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
        Ok(self.records.lock().unwrap().get(id).cloned())
    }

    async fn list(&self, filter: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError> {
        let map = self.records.lock().unwrap();
        Ok(map
            .values()
            .filter(|r| filter.status.as_deref().is_none_or(|s| r.status == s))
            .cloned()
            .collect())
    }

    async fn update(
        &self,
        id: &str,
        patch: TaskUpdatePatch,
    ) -> Result<TaskRecord, TaskRegistryError> {
        let mut map = self.records.lock().unwrap();
        let rec = map
            .get_mut(id)
            .ok_or_else(|| TaskRegistryError::NotFound(id.into()))?;
        if let Some(s) = patch.status {
            rec.status = s;
        }
        Ok(rec.clone())
    }

    async fn set_status(&self, id: &str, status: &str) -> Result<TaskRecord, TaskRegistryError> {
        let mut map = self.records.lock().unwrap();
        let rec = map
            .get_mut(id)
            .ok_or_else(|| TaskRegistryError::NotFound(id.into()))?;
        rec.status = status.into();
        Ok(rec.clone())
    }

    async fn kill(&self, id: &str) -> Result<TaskRecord, TaskRegistryError> {
        let mut map = self.records.lock().unwrap();
        let rec = map
            .get_mut(id)
            .ok_or_else(|| TaskRegistryError::NotFound(id.into()))?;
        rec.status = "killed".into();
        Ok(rec.clone())
    }

    async fn output(
        &self,
        id: &str,
        _offset: Option<u64>,
    ) -> Result<TaskOutputChunk, TaskRegistryError> {
        let map = self.records.lock().unwrap();
        if !map.contains_key(id) {
            return Err(TaskRegistryError::NotFound(id.into()));
        }
        Ok(TaskOutputChunk {
            task_id: id.into(),
            content: String::new(),
            total_lines: 0,
            truncated: false,
            ..Default::default()
        })
    }
}

// =========================================================================
// MockMailboxRouterHandle — records routed messages.
// =========================================================================

/// One captured `route` call.
#[derive(Clone, Debug)]
pub struct RoutedMessage {
    /// Sender id (string form passed to `route`).
    pub from_agent: String,
    /// Recipient id.
    pub to_agent: String,
    /// Routed message body.
    pub message: MailboxMessage,
}

/// Recording mock for `MailboxRouterHandle`.
pub struct MockMailboxRouterHandle {
    sent: Mutex<Vec<RoutedMessage>>,
    /// Locked claim-window seconds — defaults to 30 (spec §7 line 498).
    pub claim_window_secs: u64,
}

impl MockMailboxRouterHandle {
    /// Construct a mock that acks with the 30s claim window.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            claim_window_secs: 30,
        }
    }

    /// Drain and return every captured route.
    #[must_use]
    pub fn sent(&self) -> Vec<RoutedMessage> {
        self.sent.lock().unwrap().clone()
    }
}

impl Default for MockMailboxRouterHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MailboxRouterHandle for MockMailboxRouterHandle {
    async fn route(
        &self,
        from_agent: &str,
        to_agent: &str,
        message: MailboxMessage,
    ) -> Result<RouteAck, MailboxError> {
        self.sent.lock().unwrap().push(RoutedMessage {
            from_agent: from_agent.into(),
            to_agent: to_agent.into(),
            message,
        });
        Ok(RouteAck {
            claimed_at: SystemTime::now(),
            claim_window_secs: self.claim_window_secs,
        })
    }
}

// =========================================================================
// MockBudgetEnforcerHandle — atomic-counter gate.
// =========================================================================

/// Atomic-counter mock for `BudgetEnforcerHandle`. Returns `Exceeded`
/// when the running total reaches the configured cap.
pub struct MockBudgetEnforcerHandle {
    /// Cumulative nano-USD charged.
    pub total_nano_usd: AtomicU64,
    /// Cap above which `check_and_charge` returns Exceeded. `u64::MAX`
    /// effectively disables the gate.
    pub cap_nano_usd: u64,
}

impl MockBudgetEnforcerHandle {
    /// Build a mock with the supplied cap. Initial total is 0.
    #[must_use]
    pub fn new(cap_nano_usd: u64) -> Self {
        Self {
            total_nano_usd: AtomicU64::new(0),
            cap_nano_usd,
        }
    }

    /// Convenience: enforcer that will always allow charges through.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(u64::MAX)
    }

    /// Force the running total. Used by tests that want the gate to trip.
    pub fn set_total(&self, n: u64) {
        self.total_nano_usd.store(n, Ordering::SeqCst);
    }
}

#[async_trait]
impl BudgetEnforcerHandle for MockBudgetEnforcerHandle {
    async fn check_and_charge(&self, nano_usd: u64) -> Result<(), BudgetError> {
        let prev = self.total_nano_usd.fetch_add(nano_usd, Ordering::SeqCst);
        let post = prev.saturating_add(nano_usd);
        if post >= self.cap_nano_usd {
            Err(BudgetError::Exceeded {
                current_nano_usd: post,
            })
        } else {
            Ok(())
        }
    }

    async fn snapshot_total_nano_usd(&self) -> u64 {
        self.total_nano_usd.load(Ordering::SeqCst)
    }

    fn max_session_nano_usd(&self) -> Option<u64> {
        (self.cap_nano_usd != u64::MAX).then_some(self.cap_nano_usd)
    }
}

// =========================================================================
// Convenience: wrap mocks in Arcs.
// =========================================================================

/// Convenience: wrap [`MockSubagentSpawner`] in `Arc<dyn SubagentSpawner>`.
#[must_use]
pub fn arc_mock_spawner() -> Arc<MockSubagentSpawner> {
    Arc::new(MockSubagentSpawner::new())
}

/// Convenience: wrap [`MockTaskRegistryHandle`] in `Arc<dyn TaskRegistryHandle>`.
#[must_use]
pub fn arc_mock_task_registry() -> Arc<MockTaskRegistryHandle> {
    Arc::new(MockTaskRegistryHandle::new())
}

/// Convenience: wrap [`MockMailboxRouterHandle`] in `Arc<dyn MailboxRouterHandle>`.
#[must_use]
pub fn arc_mock_mailbox() -> Arc<MockMailboxRouterHandle> {
    Arc::new(MockMailboxRouterHandle::new())
}

/// Convenience: wrap [`MockBudgetEnforcerHandle`] in
/// `Arc<dyn BudgetEnforcerHandle>`.
#[must_use]
pub fn arc_mock_budget(cap: u64) -> Arc<MockBudgetEnforcerHandle> {
    Arc::new(MockBudgetEnforcerHandle::new(cap))
}
