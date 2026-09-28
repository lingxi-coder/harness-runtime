//! `LocalWorkflowHandler` — runs a model-authored workflow script and bridges
//! its `agent()` calls to real subagent spawns.
//!
//! claude-code executes workflow scripts as a JS `AsyncFunction` whose injected
//! `agent()` global spawns a child agent and resolves with its result. LingXi
//! runs the same scripts on the embedded QuickJS runtime (the `workflow` crate);
//! this module bridges that runtime's batch `agent_runner` seam to the
//! [`SubagentSpawner`] the Task subsystem uses, and wires the whole thing into a
//! background [`Task`] (mirroring [`crate::handlers::local_agent`]).
//!
//! ## The sync ↔ async bridge
//!
//! `workflow::run` is synchronous (it drives the QuickJS job queue on the
//! calling thread) and its `agent_runner` callback is invoked from sync JS, but
//! [`SubagentSpawner::spawn`] is `async` and engine code must not `block_on` the
//! runtime directly (it spawns through the `RuntimeSpawner` seam). We therefore
//! run `workflow::run` on a dedicated `std::thread` and connect it to the async
//! world with channels:
//!
//! * the script thread's batch runner sends the batch's prompts over an mpsc
//!   channel and blocks (`blocking_recv`) on a per-batch oneshot reply;
//! * an async worker loop (driven on the caller's runtime) receives each batch,
//!   spawns its subagents concurrently — bounded by [`concurrency_cap`], in the
//!   prompts' order — and sends the ordered results back over the reply channel.
//!
//! There is at most one batch in flight (the runner blocks until its reply), so
//! the channels cannot deadlock: the script only makes progress once the worker
//! has answered, and `workflow::run` only returns once every batch has been
//! answered — at which point the runner (holding the request sender) is dropped,
//! the worker loop sees the closed channel and ends, and the outcome that the
//! script thread sends last is delivered to the caller.

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use async_trait::async_trait;
use futures::stream::StreamExt;
use platform_api::filesystem::FileSystem;
use platform_api::subagent_spawn::{SelectedAgentMeta, SubagentListingEntry};
use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvokerError};
use platform_api::{
    BackgroundTaskHandle, BudgetEnforcerHandle, RuntimeSpawner, SubagentInheritance,
    SubagentResult, SubagentSpawnError, SubagentSpawnRequest, SubagentSpawner, ToolInvoker,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::id::TaskType;
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::AnalyticsBus;

// Reuse the status-sink seam defined once in the bash handler (single impl wired
// across handlers), exactly as `local_agent` does.
pub use crate::handlers::local_bash::{NoopStatusSink, TaskStatusSink};

/// Structured workflow progress payload routed alongside the existing
/// human-readable task spool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowProgressUpdate {
    pub kind: String,
    pub index: u64,
    pub title: Option<String>,
    pub message: Option<String>,
    pub label: Option<String>,
    pub phase_index: Option<u32>,
    pub phase_title: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub tool_use_id: Option<String>,
    pub queued_at_ms: Option<u64>,
    pub started_at_ms: Option<u64>,
    pub last_progress_at_ms: Option<u64>,
    pub attempt: Option<u32>,
    pub last_attempt_reason: Option<String>,
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub last_tool_name: Option<String>,
    pub last_tool_summary: Option<String>,
    pub prompt_preview: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct WorkflowRunMetrics {
    call_count: u64,
    total_tokens: u64,
    total_tool_calls: u64,
    budget_telemetry_emitted: bool,
    cap_telemetry_emitted: bool,
    agents: HashMap<u64, WorkflowAgentMetric>,
    phases: BTreeMap<u32, WorkflowPhaseMetric>,
}

#[derive(Debug, Clone)]
struct WorkflowAgentMetric {
    phase_index: Option<u32>,
    phase_title: Option<String>,
    state: &'static str,
    tokens: u64,
    tool_calls: u64,
    duration_ms: u64,
    skipped: bool,
    empty_result: bool,
}

#[derive(Debug, Clone, Default)]
struct WorkflowPhaseMetric {
    title: String,
    tokens: u64,
    tool_calls: u64,
    duration_ms: u64,
    agent_count: u64,
    error_count: u64,
    skip_count: u64,
}

impl WorkflowRunMetrics {
    fn record_phase(&mut self, index: u32, title: String) {
        self.phases
            .entry(index)
            .or_insert_with(|| WorkflowPhaseMetric {
                title,
                ..WorkflowPhaseMetric::default()
            });
    }

    fn recompute_totals(&mut self) {
        self.total_tokens = self
            .agents
            .values()
            .fold(0, |total, agent| total.saturating_add(agent.tokens));
        self.total_tool_calls = self
            .agents
            .values()
            .fold(0, |total, agent| total.saturating_add(agent.tool_calls));
    }

    fn record_cached(
        &mut self,
        index: u64,
        phase_index: Option<u32>,
        phase_title: Option<String>,
        result: &str,
    ) {
        self.agents.insert(
            index,
            WorkflowAgentMetric {
                phase_index,
                phase_title,
                state: "cached",
                tokens: 0,
                tool_calls: 0,
                duration_ms: 0,
                skipped: false,
                empty_result: workflow_result_text_is_empty(result),
            },
        );
        self.recompute_totals();
    }

    fn record_result(
        &mut self,
        index: u64,
        phase_index: Option<u32>,
        phase_title: Option<String>,
        result: &Result<SubagentResult, SubagentSpawnError>,
    ) {
        let metric = match result {
            Ok(SubagentResult::Completed {
                content,
                total_tokens,
                total_tool_use_count,
                total_duration_ms,
                ..
            }) => WorkflowAgentMetric {
                phase_index,
                phase_title,
                state: "done",
                tokens: *total_tokens,
                tool_calls: *total_tool_use_count,
                duration_ms: *total_duration_ms,
                skipped: false,
                empty_result: workflow_result_value_is_empty(content),
            },
            Ok(SubagentResult::Failed { reason, .. }) => WorkflowAgentMetric {
                phase_index,
                phase_title,
                state: "error",
                tokens: self.agents.get(&index).map_or(0, |metric| metric.tokens),
                tool_calls: self
                    .agents
                    .get(&index)
                    .map_or(0, |metric| metric.tool_calls),
                duration_ms: self
                    .agents
                    .get(&index)
                    .map_or(0, |metric| metric.duration_ms),
                skipped: reason == "skipped by user",
                empty_result: false,
            },
            Ok(SubagentResult::Killed { .. }) | Err(_) => WorkflowAgentMetric {
                phase_index,
                phase_title,
                state: "error",
                tokens: self.agents.get(&index).map_or(0, |metric| metric.tokens),
                tool_calls: self
                    .agents
                    .get(&index)
                    .map_or(0, |metric| metric.tool_calls),
                duration_ms: self
                    .agents
                    .get(&index)
                    .map_or(0, |metric| metric.duration_ms),
                skipped: false,
                empty_result: false,
            },
        };
        self.agents.insert(index, metric);
        self.recompute_totals();
    }

    fn record_progress(
        &mut self,
        index: u64,
        phase_index: Option<u32>,
        phase_title: Option<String>,
        state: Option<&str>,
        error: Option<&str>,
        tokens: Option<u64>,
        tool_calls: Option<u64>,
    ) {
        let previous = self.agents.get(&index);
        let metric = WorkflowAgentMetric {
            phase_index: phase_index.or_else(|| previous.and_then(|m| m.phase_index)),
            phase_title: phase_title.or_else(|| previous.and_then(|m| m.phase_title.clone())),
            state: match state {
                Some("done") => "done",
                Some("error") => "error",
                Some("cached") => "cached",
                _ => "progress",
            },
            tokens: tokens.or_else(|| previous.map(|m| m.tokens)).unwrap_or(0),
            tool_calls: tool_calls
                .or_else(|| previous.map(|m| m.tool_calls))
                .unwrap_or(0),
            duration_ms: previous.map_or(0, |metric| metric.duration_ms),
            skipped: error == Some("skipped by user")
                || previous.is_some_and(|metric| metric.skipped),
            empty_result: previous.is_some_and(|metric| metric.empty_result),
        };
        self.agents.insert(index, metric);
        self.recompute_totals();
    }

    fn record_duration(&mut self, index: u64, duration_ms: u64) {
        if let Some(metric) = self.agents.get_mut(&index) {
            metric.duration_ms = duration_ms;
        }
        self.recompute_totals();
    }

    fn phase_metrics(&self) -> BTreeMap<u32, WorkflowPhaseMetric> {
        let mut phases = self.phases.clone();
        for agent in self.agents.values() {
            let Some(index) = agent.phase_index else {
                continue;
            };
            let phase = phases.entry(index).or_insert_with(|| WorkflowPhaseMetric {
                title: agent.phase_title.clone().unwrap_or_default(),
                ..WorkflowPhaseMetric::default()
            });
            phase.tokens = phase.tokens.saturating_add(agent.tokens);
            phase.tool_calls = phase.tool_calls.saturating_add(agent.tool_calls);
            phase.duration_ms = phase.duration_ms.saturating_add(agent.duration_ms);
            phase.agent_count = phase.agent_count.saturating_add(1);
            if agent.state == "error" {
                if agent.skipped {
                    phase.skip_count = phase.skip_count.saturating_add(1);
                } else {
                    phase.error_count = phase.error_count.saturating_add(1);
                }
            }
        }
        phases
    }

    fn terminal_counts(&self) -> (u64, u64, u64, u64) {
        let mut done = 0_u64;
        let mut error = 0_u64;
        let mut skipped = 0_u64;
        let mut empty_result = 0_u64;
        for agent in self.agents.values() {
            match agent.state {
                "done" | "cached" => {
                    done = done.saturating_add(1);
                    if agent.empty_result {
                        empty_result = empty_result.saturating_add(1);
                    }
                }
                "error" if agent.skipped => skipped = skipped.saturating_add(1),
                "error" => error = error.saturating_add(1),
                _ => {}
            }
        }
        (done, error, skipped, empty_result)
    }
}

fn workflow_result_value_is_empty(value: &Value) -> bool {
    match value {
        Value::String(value) => workflow_result_text_is_empty(value),
        Value::Array(values) => values.is_empty(),
        Value::Object(values) => {
            values.is_empty()
                || (values.len() == 1
                    && values.values().next().is_some_and(
                        |value| matches!(value, Value::Array(items) if items.is_empty()),
                    ))
        }
        _ => false,
    }
}

fn workflow_result_text_is_empty(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    serde_json::from_str::<Value>(value)
        .ok()
        .is_some_and(|value| workflow_result_value_is_empty(&value))
}

fn workflow_agent_display_model(opts: &Value) -> Option<String> {
    let agent_model = opts.get("model").and_then(Value::as_str)?;
    let agent_model_profile = opts
        .get("modelProfile")
        .or_else(|| opts.get("model_profile"))
        .and_then(Value::as_str)
        .filter(|profile| !profile.is_empty());
    Some(platform_api::qualified_model_ref(
        agent_model,
        agent_model_profile,
    ))
}

fn format_workflow_agent_snapshot(progress: &WorkflowProgressUpdate) -> Option<String> {
    if progress.kind != "workflow_agent" {
        return None;
    }
    let mut obj = serde_json::Map::from_iter([
        ("type".to_string(), serde_json::json!("workflow_agent")),
        ("index".to_string(), serde_json::json!(progress.index)),
    ]);
    if let Some(label) = progress.label.as_ref() {
        obj.insert("label".to_string(), serde_json::json!(label));
    }
    if let Some(state) = progress.state.as_ref() {
        obj.insert("state".to_string(), serde_json::json!(state));
    }
    if let Some(phase_index) = progress.phase_index {
        obj.insert("phaseIndex".to_string(), serde_json::json!(phase_index));
    }
    if let Some(phase_title) = progress.phase_title.as_ref() {
        obj.insert("phaseTitle".to_string(), serde_json::json!(phase_title));
    }
    if let Some(agent_id) = progress.agent_id.as_ref() {
        obj.insert("agentId".to_string(), serde_json::json!(agent_id));
    }
    if let Some(agent_type) = progress.agent_type.as_ref() {
        obj.insert("agentType".to_string(), serde_json::json!(agent_type));
    }
    if let Some(model) = progress.model.as_ref() {
        obj.insert("model".to_string(), serde_json::json!(model));
    }
    if let Some(fallback_model) = progress.fallback_model.as_ref() {
        obj.insert(
            "fallbackModel".to_string(),
            serde_json::json!(fallback_model),
        );
    }
    if let Some(error) = progress.error.as_ref() {
        obj.insert("error".to_string(), serde_json::json!(error));
    }
    if let Some(tool_use_id) = progress.tool_use_id.as_ref() {
        obj.insert("toolUseID".to_string(), serde_json::json!(tool_use_id));
    }
    if let Some(queued_at_ms) = progress.queued_at_ms {
        obj.insert("queuedAt".to_string(), serde_json::json!(queued_at_ms));
    }
    if let Some(started_at_ms) = progress.started_at_ms {
        obj.insert("startedAt".to_string(), serde_json::json!(started_at_ms));
    }
    if let Some(last_progress_at_ms) = progress.last_progress_at_ms {
        obj.insert(
            "lastProgressAt".to_string(),
            serde_json::json!(last_progress_at_ms),
        );
    }
    if let Some(attempt) = progress.attempt {
        obj.insert("attempt".to_string(), serde_json::json!(attempt));
    }
    if let Some(last_attempt_reason) = progress.last_attempt_reason.as_ref() {
        obj.insert(
            "lastAttemptReason".to_string(),
            serde_json::json!(last_attempt_reason),
        );
    }
    if let Some(tokens) = progress.tokens {
        obj.insert("tokens".to_string(), serde_json::json!(tokens));
    }
    if let Some(tool_calls) = progress.tool_calls {
        obj.insert("toolCalls".to_string(), serde_json::json!(tool_calls));
    }
    if let Some(last_tool_name) = progress.last_tool_name.as_ref() {
        obj.insert(
            "lastToolName".to_string(),
            serde_json::json!(last_tool_name),
        );
    }
    if let Some(last_tool_summary) = progress.last_tool_summary.as_ref() {
        obj.insert(
            "lastToolSummary".to_string(),
            serde_json::json!(last_tool_summary),
        );
    }
    if let Some(prompt_preview) = progress.prompt_preview.as_ref() {
        obj.insert(
            "promptPreview".to_string(),
            serde_json::json!(prompt_preview),
        );
    }
    Some(format!(
        "[workflow_agent] {}",
        serde_json::Value::Object(obj)
    ))
}

fn emit_workflow_agent_snapshot(
    progress_tx: Option<&mpsc::UnboundedSender<String>>,
    progress: &WorkflowProgressUpdate,
) {
    if let (Some(tx), Some(line)) = (progress_tx, format_workflow_agent_snapshot(progress)) {
        let _ = tx.send(line);
    }
}

fn emit_workflow_agent_queued(
    progress_tx: Option<&mpsc::UnboundedSender<String>>,
    live_progress_tx: Option<&mpsc::UnboundedSender<WorkflowProgressUpdate>>,
    call_index: u64,
    label: &str,
    prompt: &str,
    phase_index: Option<u32>,
    phase_title: Option<String>,
    model: Option<String>,
    queued_at_ms: u64,
) {
    let update = WorkflowProgressUpdate {
        kind: "workflow_agent".to_string(),
        index: call_index,
        title: None,
        message: None,
        label: Some(label.to_string()),
        phase_index,
        phase_title,
        agent_id: None,
        agent_type: None,
        model,
        fallback_model: None,
        state: Some("start".to_string()),
        error: None,
        tool_use_id: Some(format!("workflow_agent_{call_index}_queued")),
        queued_at_ms: Some(queued_at_ms),
        started_at_ms: None,
        last_progress_at_ms: Some(queued_at_ms),
        attempt: Some(1),
        last_attempt_reason: None,
        tokens: None,
        tool_calls: None,
        last_tool_name: None,
        last_tool_summary: None,
        prompt_preview: Some(prompt.chars().take(120).collect()),
    };
    emit_workflow_agent_snapshot(progress_tx, &update);
    if let Some(tx) = live_progress_tx {
        let _ = tx.send(update);
    }
}

#[async_trait]
pub trait WorkflowProgressSink: Send + Sync {
    async fn emit_workflow_progress(
        &self,
        task_id: &str,
        run_id: &str,
        progress: WorkflowProgressUpdate,
    );
}

/// Handler name reported by [`Task::name`] / used as the runtime task-name.
const HANDLER_NAME: &str = "local_workflow";

fn local_app_workspace_root(data_root: &std::path::Path, app_id: &str) -> std::path::PathBuf {
    data_root.join("apps").join(app_id).join("workspace")
}

/// Whether this workflow run must hold its app's exclusive workspace lease.
///
/// Reads the task's typed [`crate::scope::LocalAppWorkflowTaskScope`]
/// (design §18 Phase -1 step 8 / §8.1) instead of matching `workflow_id`
/// against this crate's (since-deleted) `LOCAL_APP_BUILD_WORKFLOWS` array: a
/// `workflow_id` is a string the *caller* supplies when launching a
/// workflow, so a custom workflow that happens to reuse a real build
/// workflow's name used to collect the exact same lease. `None` -- no scope
/// at all -- never requires the lease; only a
/// `Some` scope whose [`LocalAppWorkflowPurpose`](crate::scope::LocalAppWorkflowPurpose)
/// is `Build` does (`LocalAppWorkflowTaskScope::requires_workspace_lease`).
///
/// `pub(crate)` (not private) so `registry_test.rs` can exercise it directly
/// alongside [`crate::registry::TaskRegistry::find_nonterminal_local_app_workflows`]
/// in the same integration test, without needing a full spawn (which needs a
/// lease registry, a data root and a live runtime).
pub(crate) fn requires_workspace_lease(
    scope: Option<&crate::scope::LocalAppWorkflowTaskScope>,
) -> bool {
    scope.is_some_and(crate::scope::LocalAppWorkflowTaskScope::requires_workspace_lease)
}

/// Claude Code `k6a` — the per-run lifetime cap on real `agent()` calls. The
/// 1001st call is refused via the throw channel so the prelude rejects the
/// `agent()` promise with `WorkflowAgentCapError` (a runaway-loop backstop).
const WORKFLOW_AGENT_CAP: u64 = 1000;

/// The `WorkflowAgentCapError` message (binary `c0p` @201953488), `${k6a}`=1000.
const WORKFLOW_AGENT_CAP_MESSAGE: &str = "Workflow agent() call cap reached (1000). This usually means a loop using budget.remaining() never terminates because no token budget was set \u{2014} remaining() returns Infinity when budget.total is null. Add a hard iteration cap to the loop, or pass a token budget.";

/// Prefix the worker prepends to a result slot to make the prelude THROW that
/// `agent()` with the rest of the string as the Error message (agent-cap or
/// budget-ceiling). U+0001-framed: collision-proof and NUL-free (the prelude
/// eval path uses a C string). MUST stay byte-identical to the prelude's
/// `__WF_THROW_PREFIX` (`String.fromCharCode(1)+"__wf_throw__"+...`).
const WF_THROW_PREFIX: &str = "\u{1}__wf_throw__\u{1}";

const WORKFLOW_EXTENSIONS: [&str; 4] = [".js", ".mjs", ".ts", ""];

const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Adds the current workflow's lease token to every recursive tool dispatch.
/// The wrapper is deliberately stateful and cannot be reused by another app.
struct WorkspaceLeaseToolInvoker {
    inner: Arc<dyn ToolInvoker>,
    token: u64,
}

#[async_trait]
impl ToolInvoker for WorkspaceLeaseToolInvoker {
    async fn invoke(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        self.inner
            .invoke_with_workspace_lease(name, input, ctx, Some(self.token))
            .await
    }

    async fn invoke_with_workspace_lease(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        _workspace_lease_token: Option<u64>,
    ) -> Result<Value, ToolInvokerError> {
        self.inner
            .invoke_with_workspace_lease(name, input, ctx, Some(self.token))
            .await
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        input: Value,
        ctx: SubagentInvocationContext,
        _workspace_lease_token: Option<u64>,
    ) -> Result<platform_api::tool_invoker::ToolInvocationResult, ToolInvokerError> {
        self.inner
            .invoke_detailed(name, input, ctx, Some(self.token))
            .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Build a throw-channel result slot carrying `message`.
fn wf_throw(message: &str) -> String {
    format!("{WF_THROW_PREFIX}{message}")
}

fn user_config_home_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(branding::DOT_DIR))
}

fn saved_workflow_candidates(name: &str) -> Vec<PathBuf> {
    let project = PathBuf::from(branding::DOT_DIR).join("workflows");
    let mut dirs = vec![project.clone()];
    if let Some(user) = user_config_home_dir().map(|home| home.join("workflows")) {
        if user != project {
            dirs.push(user);
        }
    }
    dirs.into_iter()
        .flat_map(|dir| {
            WORKFLOW_EXTENSIONS
                .iter()
                .map(move |ext| dir.join(format!("{name}{ext}")))
        })
        .collect()
}

/// Normalize the opts object for the resume chain-key (claude-code `ABp`).
///
/// The binary projects opts to ONLY `["schema","model","effort","isolation","agentType"]`.
/// LingXi additionally treats `modelProfile` as identity because its multi-provider
/// routing allows two providers to expose the same wire model id, and
/// `disallowedTools` because a plugin workflow may narrow one agent definition
/// for a particular stage. The snake-case
/// compatibility alias is canonicalized to `modelProfile`, then the projected value
/// is serialized with a recursive key-sorter. In JSON there are no functions, so we
/// skip null/absent.
///
/// IMPORTANT: `serde_json` is compiled with `preserve_order` (IndexMap-backed), so
/// `serde_json::Map` preserves INSERTION order, NOT alphabetical order. The explicit
/// sort in `sort_value` (and the fixed `KEYS`-slice iteration order for the outer map)
/// is therefore REQUIRED for determinism — do not remove it.
///
/// This means display-only fields like `phase`, `label`, `stallMs` are stripped,
/// so annotating a call differently does NOT change the key and does NOT invalidate
/// the cache on resume.
fn normalize_opts_for_chain_key(opts: &Value) -> String {
    const KEYS: &[&str] = &[
        "schema",
        "model",
        "modelProfile",
        "effort",
        "isolation",
        "agentType",
        "disallowedTools",
    ];
    let mut map = serde_json::Map::new();
    if let Some(obj) = opts.as_object() {
        for &k in KEYS {
            let value = if k == "modelProfile" {
                obj.get(k).or_else(|| obj.get("model_profile"))
            } else {
                obj.get(k)
            };
            if let Some(v) = value {
                if !v.is_null() {
                    map.insert(k.to_string(), sort_value(v.clone()));
                }
            }
        }
    }
    serde_json::to_string(&Value::Object(map)).unwrap_or_else(|_| "{}".to_string())
}

/// Recursively sort object keys so the JSON representation is deterministic
/// regardless of insertion order (mirrors the binary's `JSON.stringify` key-sorter).
///
/// IMPORTANT: `serde_json` is compiled with `preserve_order` (IndexMap-backed), so
/// `serde_json::Map` preserves INSERTION order, NOT alphabetical order. The explicit
/// sort here is REQUIRED for determinism — do not remove it.
fn sort_value(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            // Collect into a Vec, sort by key, then rebuild a Map. Since serde_json
            // uses IndexMap with preserve_order, the sort is LOAD-BEARING — without
            // it, insertion order would determine the JSON output, making chain keys
            // non-deterministic across different construction paths.
            let mut pairs: Vec<(String, Value)> = map.into_iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let sorted: serde_json::Map<String, Value> = pairs
                .into_iter()
                .map(|(k, cv)| (k, sort_value(cv)))
                .collect();
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_value).collect()),
        other => other,
    }
}

/// The chained resume-cache key for an `agent(prompt, opts)` call (claude-code
/// `qKa(se, te, m)`): a running hash that folds in the PREVIOUS key (`prev`), so
/// any change in the preceding sequence of agent() calls cascades into every
/// later key — giving "longest unchanged prefix" replay (a reorder, an inserted
/// call, or an edited prompt all break the chain from that point on). The exact
/// hash bytes are private to LingXi (the journal is its own same-session format),
/// so a stable FNV-1a-64 over `prev | prompt | opts` suffices.
fn chain_key(prev: &str, prompt: &str, opts_json: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut fold = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    fold(prev.as_bytes());
    fold(b"\x1e");
    fold(prompt.as_bytes());
    fold(b"\x1f");
    fold(opts_json.as_bytes());
    format!("{h:016x}")
}

/// Append-only workflow cache journal. Claude Code writes one `started` record
/// when the child id is allocated and one `result` record when that child
/// returns a cacheable value; a restart can therefore reuse every completed
/// prefix without waiting for the whole workflow to finish.
#[derive(Clone)]
struct WorkflowJournalWriter {
    path: PathBuf,
    fs: Arc<dyn FileSystem>,
}

impl WorkflowJournalWriter {
    async fn ensure_exists(&self) {
        if let Some(path) = self.path.to_str() {
            let _ = self.fs.append_file_with_mode(path, "", 0o600).await;
        }
    }

    async fn append_started(&self, key: &str, agent_id: &str) {
        self.append(serde_json::json!({
            "type": "started",
            "key": key,
            "agentId": agent_id,
        }))
        .await;
    }

    async fn append_result(&self, key: &str, agent_id: &str, result: &str) {
        self.append(serde_json::json!({
            "type": "result",
            "key": key,
            "agentId": agent_id,
            "result": result,
        }))
        .await;
    }

    async fn append(&self, record: Value) {
        let Some(path) = self.path.to_str() else {
            return;
        };
        if let Ok(mut line) = serde_json::to_string(&record) {
            line.push('\n');
            let _ = self.fs.append_file_with_mode(path, &line, 0o600).await;
        }
    }

    async fn load_results(&self) -> HashMap<String, String> {
        let Some(path) = self.path.to_str() else {
            return HashMap::new();
        };
        let Ok(file) = self.fs.read_file(path, None, None).await else {
            return HashMap::new();
        };
        file.content
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record.get("type").and_then(Value::as_str) == Some("result"))
            .filter_map(|record| {
                Some((
                    record.get("key")?.as_str()?.to_string(),
                    record.get("result")?.as_str()?.to_string(),
                ))
            })
            .collect()
    }
}

/// `WorkflowBudgetExceededError` message (binary `I6a` @201953813), with
/// thousands-separated counts (`toLocaleString`).
fn workflow_budget_exceeded_message(spent: u64, total: u64) -> String {
    format!(
        "Workflow token budget exceeded ({} / {} output tokens). Stopping further agent() calls. In-flight agents will complete; their results are preserved.",
        group_en_us(spent),
        group_en_us(total),
    )
}

/// `Number.prototype.toLocaleString()` for en-US: group integer digits in 3s
/// with commas.
fn group_en_us(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// The subagent type spawned for a bare `agent(prompt)` call — claude-code's
/// default workflow subagent.
pub const DEFAULT_WORKFLOW_SUBAGENT: &str = "workflow-subagent";

enum WorkflowBridgeRequest {
    AgentBatch(Vec<(String, String)>, oneshot::Sender<Vec<String>>),
}

/// A live worker-cancel record: the background-task handle plus the runtime that
/// minted it, so [`Task::kill`] / cleanup can cancel the in-flight worker without
/// a fresh [`TaskContext`]. (Each handler keeps its own — the fields are private;
/// `pub` so it can appear in [`LocalWorkflowHandler::workers_map`]'s return type.)
pub struct WorkerCancel {
    handle: BackgroundTaskHandle,
    runtime: Arc<dyn RuntimeSpawner>,
    /// Cooperative-cancel flag shared with the running script thread's engine
    /// interrupt handler. Flipped by [`Task::kill`] / cleanup so a runaway
    /// pure-JS loop aborts instead of leaking the OS thread.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Worker completion signal. `kill` waits briefly on this after flipping
    /// the cooperative cancel flag so an in-flight batch can unwind before the
    /// runtime fallback aborts the task.
    completion_rx: StdMutex<Option<oneshot::Receiver<()>>>,
    /// Natural completion owns the terminal transition once this flips. Kill
    /// and cleanup must not abort the outcome/status commit window.
    finalizing: bool,
}

struct WorkerCompletionSignal(Option<oneshot::Sender<()>>);

impl WorkerCompletionSignal {
    fn new(tx: oneshot::Sender<()>) -> Self {
        Self(Some(tx))
    }
}

impl Drop for WorkerCompletionSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

async fn cancel_workflow_worker(rec: WorkerCancel) -> Result<(), TaskError> {
    rec.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut completion_rx = rec
        .completion_rx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let completed = if let Some(completion_rx) = completion_rx.as_mut() {
        tokio::time::timeout(std::time::Duration::from_secs(2), completion_rx)
            .await
            .is_ok()
    } else {
        false
    };
    if !completed {
        rec.runtime
            .cancel(&rec.handle)
            .await
            .map_err(|error| TaskError::Io(error.to_string()))?;
        if let Some(completion_rx) = completion_rx {
            let _ = completion_rx.await;
        }
    }
    Ok(())
}

/// Background [`Task`] that runs a workflow script and spools its result.
///
/// Mirrors [`crate::handlers::local_agent::LocalAgentHandler`]: a runtime-spawned
/// worker drives the script to completion (via [`run_workflow_script`]), spools
/// the script's return value, reports the terminal status, and removes its own
/// cancel record on exit. `kill` cancels the in-flight worker.
type ProcessOwners = Arc<StdMutex<HashSet<protocol::AgentId>>>;

/// Runs workflow workers and retains the process identities of their subagents.
pub struct LocalWorkflowHandler {
    process_owners: StdMutex<HashMap<String, ProcessOwners>>,
    /// Allocates a subagent slot and pumps it to a terminal [`SubagentResult`].
    spawner: Arc<dyn SubagentSpawner>,
    /// Parent's tool invoker — passed through *unchanged* in
    /// [`SubagentInheritance`] (the recursion lock relies on `Arc::ptr_eq`).
    tool_invoker: Arc<dyn ToolInvoker>,
    /// Parent's budget enforcer — passed through *unchanged* so budget charges
    /// aggregate across the whole agent tree (`Arc::ptr_eq` invariant).
    budget: Arc<dyn BudgetEnforcerHandle>,
    /// Owns the spool directory + path allocation for the result payload.
    output_manager: Arc<TaskOutputManager>,
    /// Where terminal status transitions are reported.
    status_sink: Arc<dyn TaskStatusSink>,
    /// Optional structured workflow-progress sink for realtime client updates.
    workflow_progress_sink: Option<Arc<dyn WorkflowProgressSink>>,
    /// Optional worktree manager used to realize workflow `agent(...,
    /// {isolation:"worktree"})` calls. When wired, each isolated workflow
    /// subagent gets a fresh worktree cwd and the terminal keep/cleanup
    /// judgment runs after the spawn returns.
    worktree_manager: Option<Arc<dyn platform_api::worktree::WorktreeManager>>,
    /// `task_id` → live worker-cancel record (removed by the worker on exit, or
    /// by [`Task::kill`] / cleanup).
    workers: Arc<Mutex<HashMap<String, WorkerCancel>>>,
    /// Task ids queued for teardown by the synchronous [`TaskHandle::cleanup`]
    /// closure. The closure cannot await and must not drop a cancellation
    /// request on lock contention, so it records the task id here and
    /// [`Self::drain_pending_kills`] later resolves the live worker handle.
    pending_kill: Arc<StdMutex<Vec<String>>>,
    /// Analytics bus for emitting `tengu_workflow_*` telemetry events.
    bus: Arc<AnalyticsBus>,
    /// The turn's token target (`cfg.token_budget`) backing the script's
    /// `budget.total`. `None` ⇒ no target (FLEET defaults, budget loops skip).
    /// Set by the root via [`Self::with_token_budget`].
    token_budget_total: Option<u64>,
    /// Late-bound shared output-token pool backing the script's `budget.spent()`.
    /// The composition root publishes the orchestrator's
    /// `output_token_pool` here once it exists (the handler is registered before
    /// the orchestrator is built — same deferred pattern as the tool invoker).
    /// When set, a run's subagents add their output tokens to this same `Arc`
    /// that the main loop also feeds, so `spent()` reads main loop + all
    /// workflows. When unset (tests), a run falls back to its own private pool.
    output_pool_cell: Option<Arc<OnceLock<Arc<AtomicU64>>>>,
    output_scopes: Option<Arc<dyn platform_api::WorkflowOutputScopes>>,
    /// Late-bound turn-start output baseline (claude-code `xtr`): the cumulative
    /// output at the start of the CURRENT turn. The composition root publishes
    /// the orchestrator's `turn_start_output_baseline` here. At spawn the handler
    /// snapshots `baseline.load()` so the run's `budget.spent()` is turn-relative
    /// (`pool - baseline` = `getTurnSpent()`). Unset (tests) ⇒ baseline 0.
    turn_baseline_cell: Option<Arc<OnceLock<Arc<AtomicU64>>>>,
    /// Optional permission lease registry used by local-app build workflows.
    workspace_leases: Option<Arc<permission::WorkspacePermissionLeaseRegistry>>,
    /// Profile root containing `apps/<app_id>/workspace`. The local-app
    /// workflow derives the exact app workspace from its validated `app_id`
    /// instead of reusing the engine session cwd (which may belong to another
    /// app or to the host project).
    workspace_root: Option<std::path::PathBuf>,
    /// Optional live plugin-workflow registry (§14 — the SAME `Arc` shared
    /// with `plugin::PluginManager::with_plugin_workflows` and
    /// `tool_workflow::WorkflowTool::with_plugin_workflows`). When wired, a
    /// nested `workflow({name})` call inside a running script can resolve a
    /// plugin's saved workflow by its namespaced name, after the project/user
    /// saved-workflow directories have already missed (see
    /// [`resolve_nested_script`]).
    plugin_workflows: Option<Arc<workflow::PluginWorkflowRegistry>>,
}

impl LocalWorkflowHandler {
    /// Construct a handler with the injected dependencies. `tool_invoker` +
    /// `budget` are stored so each run can bundle them into a
    /// [`SubagentInheritance`] (cloning the `Arc` preserves pointer identity —
    /// required by the recursion-lock + budget-aggregation invariants).
    #[must_use]
    pub fn new(
        spawner: Arc<dyn SubagentSpawner>,
        tool_invoker: Arc<dyn ToolInvoker>,
        budget: Arc<dyn BudgetEnforcerHandle>,
        output_manager: Arc<TaskOutputManager>,
    ) -> Self {
        Self {
            process_owners: StdMutex::new(HashMap::new()),
            spawner,
            tool_invoker,
            budget,
            output_manager,
            status_sink: Arc::new(NoopStatusSink),
            workflow_progress_sink: None,
            worktree_manager: None,
            workers: Arc::new(Mutex::new(HashMap::new())),
            pending_kill: Arc::new(StdMutex::new(Vec::new())),
            bus: Arc::new(AnalyticsBus::new()),
            token_budget_total: None,
            output_pool_cell: None,
            output_scopes: None,
            turn_baseline_cell: None,
            workspace_leases: None,
            workspace_root: None,
            plugin_workflows: None,
        }
    }

    /// Share the host's live plugin-workflow registry (the SAME `Arc` handed
    /// to `plugin::PluginManager::with_plugin_workflows` and
    /// `tool_workflow::WorkflowTool::with_plugin_workflows`).
    #[must_use]
    pub fn with_plugin_workflows(
        mut self,
        registry: Arc<workflow::PluginWorkflowRegistry>,
    ) -> Self {
        self.plugin_workflows = Some(registry);
        self
    }

    /// Composition-test seam for asserting the nested resolver shares the
    /// host's one live plugin-workflow table.
    #[doc(hidden)]
    #[must_use]
    pub fn shares_plugin_workflows(
        &self,
        registry: &Arc<workflow::PluginWorkflowRegistry>,
    ) -> bool {
        self.plugin_workflows
            .as_ref()
            .is_some_and(|wired| Arc::ptr_eq(wired, registry))
    }
    /// Attach a [`TaskStatusSink`] so terminal transitions are reported.
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn TaskStatusSink>) -> Self {
        self.status_sink = sink;
        self
    }

    /// Attach a structured workflow-progress sink for realtime client updates.
    #[must_use]
    pub fn with_workflow_progress_sink(mut self, sink: Arc<dyn WorkflowProgressSink>) -> Self {
        self.workflow_progress_sink = Some(sink);
        self
    }

    /// Wire workflow-agent worktree isolation.
    #[must_use]
    pub fn with_worktree_manager(
        mut self,
        manager: Arc<dyn platform_api::worktree::WorktreeManager>,
    ) -> Self {
        self.worktree_manager = Some(manager);
        self
    }

    /// Attach an [`AnalyticsBus`] so `tengu_workflow_*` events are emitted.
    /// Without this, a no-op bus is used (default).
    #[must_use]
    pub fn with_bus(mut self, bus: Arc<AnalyticsBus>) -> Self {
        self.bus = bus;
        self
    }

    /// Set the turn's token target (`cfg.token_budget`) so the script's
    /// `budget.total`/`remaining()` reflect it (`spent()` is the run's own
    /// accumulated subagent output tokens).
    #[must_use]
    pub fn with_token_budget(mut self, total: Option<u64>) -> Self {
        self.token_budget_total = total;
        self
    }

    /// Late-bind the shared output-token pool backing `budget.spent()`. The
    /// composition root passes a cloned `OnceLock` cell here at registration and
    /// publishes the orchestrator's `output_token_pool` into it once the
    /// orchestrator is built — so a run's `spent()` reflects the union of
    /// main-loop and all-workflow output tokens. See [`Self::output_pool_cell`].
    #[must_use]
    pub fn with_output_pool_cell(mut self, cell: Arc<OnceLock<Arc<AtomicU64>>>) -> Self {
        self.output_pool_cell = Some(cell);
        self
    }

    /// Use the host's shared session/turn output authority for new runs.
    #[must_use]
    pub fn with_output_scopes(
        mut self,
        scopes: Arc<dyn platform_api::WorkflowOutputScopes>,
    ) -> Self {
        self.output_scopes = Some(scopes);
        self
    }

    /// Late-bind the turn-start output baseline cell (claude-code `xtr`). The
    /// composition root publishes the orchestrator's `turn_start_output_baseline`
    /// so each run's `budget.spent()` is turn-relative. See
    /// [`Self::turn_baseline_cell`].
    #[must_use]
    pub fn with_turn_baseline_cell(mut self, cell: Arc<OnceLock<Arc<AtomicU64>>>) -> Self {
        self.turn_baseline_cell = Some(cell);
        self
    }

    #[must_use]
    pub fn with_workspace_permission_leases(
        mut self,
        registry: Arc<permission::WorkspacePermissionLeaseRegistry>,
        app_data_root: std::path::PathBuf,
    ) -> Self {
        self.workspace_leases = Some(registry);
        self.workspace_root = Some(app_data_root);
        self
    }

    /// Share the same `workers` map with an external owner (registry wiring) so a
    /// [`TaskHandle::cleanup`] closure and [`Task::kill`] observe the same handles.
    #[must_use]
    pub fn workers_map(&self) -> Arc<Mutex<HashMap<String, WorkerCancel>>> {
        self.workers.clone()
    }

    /// Drain records queued by [`TaskHandle::cleanup`] and cancel each worker
    /// future for real (the async counterpart of the synchronous cleanup closure).
    pub async fn drain_pending_kills(&self) {
        let pending = {
            let mut pending = self.pending_kill.lock().unwrap();
            std::mem::take(&mut *pending)
        };
        for task_id in pending {
            let rec = {
                let mut workers = self.workers.lock().await;
                if workers.get(&task_id).is_some_and(|rec| rec.finalizing) {
                    None
                } else {
                    workers.remove(&task_id)
                }
            };
            let Some(rec) = rec else {
                continue;
            };
            let _ = cancel_workflow_worker(rec).await;
            if !self.status_sink.is_terminal(&task_id).await {
                self.status_sink
                    .set_status(&task_id, TaskStatus::Killed)
                    .await;
            }
        }
    }
}

struct WorkflowIsolationSpawner {
    process_owners: ProcessOwners,
    inner: Arc<dyn SubagentSpawner>,
    worktree: Option<Arc<dyn platform_api::worktree::WorktreeManager>>,
    slug_prefix: String,
    sequence: AtomicU64,
    transcript_subdir: Option<PathBuf>,
}

impl WorkflowIsolationSpawner {
    async fn spawn_inner(
        &self,
        mut request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
        watchdog: Option<platform_api::subagent_spawn::WorkflowQueryWatchdog>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        // Process cleanup depends on synchronous allocation receipts, not on
        // whether a live-progress UI or journal happens to be enabled.
        let observer = Some(Arc::new(WorkflowProcessObserver {
            owners: self.process_owners.clone(),
            inner: observer,
        })
            as Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>);
        let worktree = if request.isolation.as_deref() == Some("worktree") {
            if let Some(manager) = self.worktree.as_ref() {
                let seq = self
                    .sequence
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let slug = format!("workflow-agent-{}-{seq}", self.slug_prefix);
                let handle = manager
                    .create_worktree(&slug, None, &[])
                    .await
                    .map_err(|e| {
                        SubagentSpawnError::Runtime(format!(
                            "Cannot create workflow agent worktree: {e}"
                        ))
                    })?;
                if request.cwd.is_none() {
                    request.cwd = Some(handle.path.to_string_lossy().into_owned());
                }
                request.worktree = Some(handle.clone());
                Some(handle)
            } else {
                // Mobile and other minimal builds document `isolation:"worktree"`
                // as a plain spawn fallback when no worktree manager is wired.
                None
            }
        } else {
            request.worktree.clone()
        };

        let result =
            agent::with_transcript_subdir_override(self.transcript_subdir.clone(), async {
                match watchdog {
                    Some(policy) => {
                        self.inner
                            .spawn_workflow_with_observer(
                                request, inherit, progress, observer, policy,
                            )
                            .await
                    }
                    None => {
                        self.inner
                            .spawn_with_observer(request, inherit, progress, observer)
                            .await
                    }
                }
            })
            .await;
        if let (Some(manager), Some(handle)) = (self.worktree.as_ref(), worktree.as_ref()) {
            let _ = platform_api::worktree::agent_worktree_result(manager.as_ref(), handle).await;
        }
        result
    }
}

struct WorkflowProcessObserver {
    owners: ProcessOwners,
    inner: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
}

#[async_trait]
impl platform_api::subagent_spawn::SubagentSpawnObserver for WorkflowProcessObserver {
    async fn on_model_selected(
        &self,
        event: &platform_api::subagent_spawn::SubagentObservation,
        effort: Option<&str>,
    ) {
        if let Some(inner) = &self.inner {
            inner.on_model_selected(event, effort).await;
        }
    }

    async fn before_start(
        &self,
        event: &platform_api::subagent_spawn::SubagentObservation,
    ) -> Result<(), SubagentSpawnError> {
        if let Some(inner) = &self.inner {
            inner.before_start(event).await?;
        }
        Ok(())
    }
    fn on_allocated(&self, event: &platform_api::subagent_spawn::SubagentObservation) {
        if let platform_api::subagent_spawn::SubagentObservation::Allocated { agent_id, .. } = event
        {
            self.owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(*agent_id);
        }
        if let Some(inner) = &self.inner {
            inner.on_allocated(event);
        }
    }

    async fn on_event(&self, event: platform_api::subagent_spawn::SubagentObservation) {
        if let platform_api::subagent_spawn::SubagentObservation::Allocated { agent_id, .. } =
            &event
        {
            self.owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(*agent_id);
        }
        if let Some(inner) = &self.inner {
            inner.on_event(event).await;
        }
    }
}

async fn wait_for_workflow_registration(
    status_sink: &Arc<dyn TaskStatusSink>,
    runtime: &Arc<dyn RuntimeSpawner>,
    task_id: &str,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    let start = std::time::Instant::now();
    while !status_sink.is_registered(task_id).await {
        if cancel.load(std::sync::atomic::Ordering::Relaxed)
            || start.elapsed() >= REGISTRATION_TIMEOUT
        {
            return false;
        }
        runtime.sleep(std::time::Duration::from_millis(1)).await;
    }
    true
}

#[async_trait]
impl SubagentSpawner for WorkflowIsolationSpawner {
    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn_inner(request, inherit, None, None, None).await
    }

    async fn spawn_with_progress(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn_inner(request, inherit, progress, None, None)
            .await
    }

    async fn spawn_with_observer(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn_inner(request, inherit, progress, observer, None)
            .await
    }

    async fn spawn_workflow_with_observer(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
        watchdog: platform_api::subagent_spawn::WorkflowQueryWatchdog,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn_inner(request, inherit, progress, observer, Some(watchdog))
            .await
    }

    async fn agent_listing(&self) -> Vec<SubagentListingEntry> {
        self.inner.agent_listing().await
    }

    /// Delegated alongside `agent_listing` so the workflow-isolated pool and the
    /// catalog it advertises agree on which agents are actually available
    /// (claude 2.1.238 `NJa`, @290291941).
    async fn tools_denied_agent_types(&self) -> Vec<String> {
        self.inner.tools_denied_agent_types().await
    }

    async fn resolve_required_mcp_servers(&self, subagent_type: &str) -> Vec<String> {
        self.inner.resolve_required_mcp_servers(subagent_type).await
    }

    async fn resolve_selection(
        &self,
        subagent_type: &str,
        model: Option<&str>,
    ) -> SelectedAgentMeta {
        self.inner.resolve_selection(subagent_type, model).await
    }

    async fn register_name(&self, name: &str, agent_id: protocol::AgentId) {
        self.inner.register_name(name, agent_id).await;
    }

    async fn resolve_name(&self, name: &str) -> Option<protocol::AgentId> {
        self.inner.resolve_name(name).await
    }

    async fn spawn_async(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<platform_api::subagent_spawn::AsyncLaunch, SubagentSpawnError> {
        self.inner.spawn_async(request, inherit).await
    }
}

/// The env var that raises or lowers the workflow's in-flight `agent()` gate
/// for one run, above (or below) the CPU-derived default.
const WORKFLOW_MAX_CONCURRENT_AGENTS_ENV: &str = "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS";

/// claude-code's DEFAULT concurrency cap for in-flight `agent()` calls —
/// `lr(ir())` where `function lr(e){return Math.min(16,Math.max(2,e-2))}`
/// and `ir()` is the CPU count. At least 2, at most 16.
fn concurrency_cap_default(cores: usize) -> usize {
    cores.saturating_sub(2).max(2).min(16)
}

/// Parse the `CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS` override, or `None`
/// when there is nothing usable to apply.
///
/// The env proxy runs the schema entry `int({min:1,max:256,digitsOnly:!0})`,
/// whose transform is:
///
/// ```js
/// if (e === void 0) return;
/// if (n?.digitsOnly && !/^[+-]?\d+$/.test(e.trim())) return;
/// let i = Sl(e);
/// if (!Number.isFinite(i)) return;
/// if (n?.min !== void 0 && i < n.min) return;
/// if (n?.max !== void 0 && i > n.max) return;
/// return i;
/// ```
///
/// Every rejection path returns `undefined` — NOT an error and NOT a clamp.
/// `let Kt = a.CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS ?? ur` then takes the
/// CPU default, so a typo, a non-integer, or a value outside 1..=256 silently
/// keeps the default rather than failing the run or quietly running at the
/// nearest legal value. Both alternatives would be worse: a failed run loses
/// the work, and a clamp would honour a request the user did not make.
fn parse_concurrency_cap_override(raw: &str) -> Option<usize> {
    let trimmed = raw.trim();
    // `/^[+-]?\d+$/` against the TRIMMED string: a leading sign is allowed
    // here and rejected below by `min`, and surrounding whitespace is fine.
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // A digit run too long for `i64` is `Number.isFinite` in JS but lands far
    // above `max`, so both spellings reach the same rejection.
    let value = trimmed.parse::<i64>().ok()?;
    if !(1..=256).contains(&value) {
        return None;
    }
    usize::try_from(value).ok()
}

/// Compose the default with the override — oracle
/// `let Kt = a.CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS ?? ur`.
///
/// Pure so the composition itself is pinned: reading the env here instead
/// would leave "the override is parsed but never applied" untestable without
/// `set_var`, which makes the parallel suite flake.
fn resolve_concurrency_cap(raw: Option<&str>, cores: usize) -> usize {
    let default = concurrency_cap_default(cores);
    let override_value = raw.and_then(parse_concurrency_cap_override);
    // `if(a.CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS!==void 0)` — `a` is the
    // PARSED proxy, so this logs exactly when the override was accepted. An
    // unusable value says nothing and runs at the default.
    if let Some(cap) = override_value {
        tracing::debug!(
            "workflow: concurrent agent gate = {cap} ({WORKFLOW_MAX_CONCURRENT_AGENTS_ENV})"
        );
    }
    override_value.unwrap_or(default)
}

/// claude-code's concurrency cap for in-flight `agent()` calls, after the
/// per-run env override.
fn concurrency_cap() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    resolve_concurrency_cap(
        std::env::var(WORKFLOW_MAX_CONCURRENT_AGENTS_ENV)
            .ok()
            .as_deref(),
        cores,
    )
}

/// JavaScript `String.length` counts UTF-16 code units, which is the value
/// Claude Code records as `script_size_chars` in `tengu_workflow_launched`.
fn workflow_script_size_chars(script: &str) -> i64 {
    script.encode_utf16().count() as i64
}

/// Claude Code `Fqe(source, scriptIsVerbatimBuiltIn)`: only verbatim bundled
/// scripts keep their real name/description in tengu payloads. Everyone else
/// is `"custom"` / `""` (`Ytr` / `Ztr` @2.1.245).
fn telemetry_is_verbatim_builtin(source: Option<&str>, script_is_verbatim: Option<bool>) -> bool {
    source == Some("built-in") && script_is_verbatim.unwrap_or(true)
}

/// `Ytr`: builtin+verbatim with a name → that name, otherwise `"custom"`.
fn telemetry_workflow_name(
    source: Option<&str>,
    script_is_verbatim: Option<bool>,
    name: Option<&str>,
) -> String {
    if telemetry_is_verbatim_builtin(source, script_is_verbatim) {
        name.filter(|value| !value.is_empty())
            .unwrap_or("custom")
            .to_string()
    } else {
        "custom".to_string()
    }
}

/// `Ztr` / `uns = 200`: builtin+verbatim descriptions are `slice(0, 200)` in
/// UTF-16 code units; everything else is `""`.
fn telemetry_workflow_description(
    source: Option<&str>,
    script_is_verbatim: Option<bool>,
    description: Option<&str>,
) -> String {
    if !telemetry_is_verbatim_builtin(source, script_is_verbatim) {
        return String::new();
    }
    let raw = description.unwrap_or("");
    let units: Vec<u16> = raw.encode_utf16().take(200).collect();
    String::from_utf16_lossy(&units)
}

fn structured_output_parse_retries(opts: &Value) -> Result<u32, &'static str> {
    match opts.get("structuredOutputParseRetries") {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= 2)
            .map(|value| value as u32)
            .ok_or("agent({structuredOutputParseRetries}): expected an integer from 0 to 2"),
    }
}

/// Build the `SubagentSpawnRequest` for one `agent(prompt, opts)` call. The
/// spawn-affecting `agent()` opts are mapped from `opts_json`
/// (`JSON.stringify(opts)`): `agentType` overrides the default subagent type,
/// `model` and `isolation` pass through, and `label` becomes the subagent's
/// display name (claude-code `re = opts.label ?? prompt.slice(0,60)`, surfaced
/// as the agent's progress label). (`schema` is handled below; the `phase` opt
/// groups the agent in claude's /workflows progress tree, for which LingXi has
/// no per-agent progress surface — only `phase()`/`log()` lines — so it has no
/// mapping target here.)
fn make_request(
    default_subagent_type: &str,
    prompt: &str,
    opts_json: &str,
) -> SubagentSpawnRequest {
    let opts: Value = serde_json::from_str(opts_json).unwrap_or(Value::Null);
    let opt_str = |k: &str| {
        opts.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let subagent_type = opt_str("agentType").unwrap_or_else(|| default_subagent_type.to_string());
    let model = opt_str("model");
    let model_profile = opt_str("modelProfile").or_else(|| opt_str("model_profile"));

    // Workflow agent() routing (binary §§1-4 + §6):
    // Case 1: bare agent(prompt) → workflow-subagent + kBp (subagent_type already =
    //   "workflow-subagent" from DEFAULT_WORKFLOW_SUBAGENT; no override needed)
    // Case 2: agent(prompt, {schema}) only → workflow-subagent + xBp (override kBp prompt)
    // Case 3: agent(prompt, {agentType}) → that type + HBp addendum + disallow union
    // Case 4: agent(prompt, {agentType, schema}) → that type + IBp addendum + disallow union
    let has_agent_type = opt_str("agentType").is_some();
    let has_schema = opts.get("schema").filter(|v| !v.is_null()).is_some();

    let system_prompt_override = if !has_agent_type && has_schema {
        // Case 2: bare schema call → xBp replaces kBp
        Some(agent::builtins::WORKFLOW_SUBAGENT_SCHEMA_PROMPT.to_string())
    } else {
        None
    };

    let system_prompt_addendum = if has_agent_type {
        if has_schema {
            // Case 4: user agentType + schema → IBp addendum
            Some(agent::builtins::WORKFLOW_SUBAGENT_SCHEMA_ADDENDUM.to_string())
        } else {
            // Case 3: user agentType no schema → HBp addendum
            Some(agent::builtins::WORKFLOW_SUBAGENT_NON_SCHEMA_ADDENDUM.to_string())
        }
    } else {
        None
    };

    let mut additional_disallowed_tools = if has_agent_type {
        // Cases 3/4: union disallowed with {SendUserMessage, Agent, Workflow}
        agent::builtins::workflow_subagent_disallowed()
    } else {
        vec![]
    };
    if let Some(stage_denies) = opts.get("disallowedTools").and_then(Value::as_array) {
        for name in stage_denies.iter().filter_map(Value::as_str) {
            if !name.is_empty()
                && !additional_disallowed_tools
                    .iter()
                    .any(|value| value == name)
            {
                additional_disallowed_tools.push(name.to_string());
            }
        }
    }

    SubagentSpawnRequest {
        teammate_color: None,
        subagent_type,
        prompt: prompt.to_string(),
        observer: None,
        context_paths: Vec::new(),
        description: None,
        model,
        model_profile,
        run_in_background: false,
        // `agent(prompt, { label })` → the subagent's display label.
        name: opt_str("label"),
        team_name: None,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
        mode: None,
        isolation: opt_str("isolation"),
        cwd: None,
        worktree: None,
        fork_context_messages: None,
        fork_parent_system_prompt: None,
        // `agent(prompt, { schema })` → structured output. The opt is a JSON
        // Schema object; carry it as its serialised form for the runner to force.
        schema: opts
            .get("schema")
            .filter(|v| !v.is_null())
            .map(std::string::ToString::to_string),
        // Workflow `agent({schema})` keeps the pre-existing forced-every-turn
        // contract (byte-parity with pre-WP2a behavior).
        structured_output_mode: platform_api::subagent_spawn::StructuredOutputMode::Forced,
        structured_output_parse_retries: structured_output_parse_retries(&opts).unwrap_or(0),
        // `agent(prompt, { effort })` → override the subagent's thinking effort
        // (claude-code `me={...ie,effort:ae}`). A level string or integer, carried
        // raw for the spawner to apply onto the resolved agent definition.
        effort: opts.get("effort").filter(|v| !v.is_null()).cloned(),
        // Workflow `agent()` spawns are not tool-call-originated background tasks.
        tool_use_id: None,
        system_prompt_override,
        system_prompt_addendum,
        additional_disallowed_tools,
        depth: 0,
        origin_session_id: None,
        // Workflow-spawned agents are top-level ⇒ the spawner's default anchors.
        parent_model_override: None,
        forked_skill_name: None,
        forked_skill_attribution: None,
        forked_skill_effort: None,
        frozen_command_denies: Vec::new(),
        resumed_history: None,
        max_turns_override: None,
        max_output_tokens_per_turn: None,
        max_input_bytes_per_turn: None,
        query_source_label: None,
        correlation_id: None,
        model_attempt: None,
    }
}

/// Render a subagent's JSON `content` payload as the string the workflow's
/// `agent()` resolves with. A bare JSON string resolves to its inner text
/// (claude-code: `agent()` without a schema returns the subagent's final text);
/// any other shape (an object/array from a structured `schema` run) is
/// serialised verbatim so the script can `JSON.parse` it.
fn value_to_text(content: Value) -> String {
    match content {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

/// Map a terminal subagent result to the string the runner returns for that
/// prompt. A failed/killed/errored agent maps to the NULL sentinel, which the
/// prelude resolves the `agent()` promise with `null` — claude-code's contract
/// (`if (Be.skipped) return null` / `if (Be.apiError) return null`). `null` is
/// falsy, so `.filter(Boolean)` still drops it, while explicit `=== null` / `??`
/// checks now behave correctly. (An empty-but-successful agent still returns "".)
fn result_to_string(result: Result<SubagentResult, SubagentSpawnError>) -> String {
    match result {
        Ok(SubagentResult::Completed { content, .. }) => value_to_text(content),
        Ok(SubagentResult::Failed { .. } | SubagentResult::Killed { .. }) | Err(_) => {
            workflow::WF_NULL_SENTINEL.to_string()
        }
    }
}

fn subagent_failure_reason(result: &Result<SubagentResult, SubagentSpawnError>) -> Option<String> {
    match result {
        Ok(SubagentResult::Completed { .. }) => None,
        Ok(SubagentResult::Failed { reason, .. }) => Some(reason.clone()),
        Ok(SubagentResult::Killed { .. }) => Some("subagent was cancelled".to_string()),
        Err(error) => Some(error.to_string()),
    }
}

/// Render a live `phase()`/`log()`/`agent` progress event as a task-output line.
///
/// `Phase` and `Log` render as human-readable text lines (back-compat).
/// `Agent` events serialize as a JSON line prefixed `[workflow_agent] ` —
/// this provides structured data in the task-output spool while keeping the
/// format readable (and parseable by consumers looking for `[workflow_agent]`).
fn format_progress(p: &workflow::Progress) -> String {
    match p {
        workflow::Progress::Phase { index, title } => format!("[{index}] === {title} ==="),
        workflow::Progress::Log { message } => message.clone(),
        workflow::Progress::Agent {
            index,
            label,
            phase_index,
            phase_title,
            agent_id,
            model,
            state,
            error,
            tool_use_id,
        } => {
            let mut obj = serde_json::json!({
                "type": "workflow_agent",
                "index": index,
                "label": label,
                "state": state.as_str(),
                "toolUseID": tool_use_id,
            });
            if let Some(obj_map) = obj.as_object_mut() {
                if let Some(pi) = phase_index {
                    obj_map.insert("phaseIndex".to_string(), serde_json::json!(pi));
                }
                if let Some(pt) = phase_title {
                    obj_map.insert("phaseTitle".to_string(), serde_json::json!(pt));
                }
                if let Some(id) = agent_id {
                    obj_map.insert("agentId".to_string(), serde_json::json!(id));
                }
                if let Some(m) = model {
                    obj_map.insert("model".to_string(), serde_json::json!(m));
                }
                if let Some(error) = error {
                    obj_map.insert("error".to_string(), serde_json::json!(error));
                }
            }
            format!(
                "[workflow_agent] {}",
                serde_json::to_string(&obj).unwrap_or_default()
            )
        }
    }
}

fn unix_time_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn workflow_progress_update(progress: &workflow::Progress) -> WorkflowProgressUpdate {
    match progress {
        workflow::Progress::Phase { index, title } => WorkflowProgressUpdate {
            kind: "workflow_phase".to_string(),
            index: u64::from(*index),
            title: Some(title.clone()),
            message: None,
            label: None,
            phase_index: Some(*index),
            phase_title: Some(title.clone()),
            agent_id: None,
            agent_type: None,
            model: None,
            fallback_model: None,
            state: None,
            error: None,
            tool_use_id: None,
            queued_at_ms: None,
            started_at_ms: None,
            last_progress_at_ms: Some(unix_time_ms_now()),
            attempt: None,
            last_attempt_reason: None,
            tokens: None,
            tool_calls: None,
            last_tool_name: None,
            last_tool_summary: None,
            prompt_preview: None,
        },
        workflow::Progress::Log { message } => WorkflowProgressUpdate {
            kind: "workflow_log".to_string(),
            index: 0,
            title: None,
            message: Some(message.clone()),
            label: None,
            phase_index: None,
            phase_title: None,
            agent_id: None,
            agent_type: None,
            model: None,
            fallback_model: None,
            state: None,
            error: None,
            tool_use_id: None,
            queued_at_ms: None,
            started_at_ms: None,
            last_progress_at_ms: Some(unix_time_ms_now()),
            attempt: None,
            last_attempt_reason: None,
            tokens: None,
            tool_calls: None,
            last_tool_name: None,
            last_tool_summary: None,
            prompt_preview: None,
        },
        workflow::Progress::Agent {
            index,
            label,
            phase_index,
            phase_title,
            agent_id,
            model,
            state,
            error,
            tool_use_id,
        } => WorkflowProgressUpdate {
            kind: "workflow_agent".to_string(),
            index: *index,
            title: None,
            message: None,
            label: Some(label.clone()),
            phase_index: *phase_index,
            phase_title: phase_title.clone(),
            agent_id: agent_id.clone(),
            agent_type: None,
            model: model.clone(),
            fallback_model: None,
            state: Some(state.as_str().to_string()),
            error: error.clone(),
            tool_use_id: Some(tool_use_id.clone()),
            queued_at_ms: None,
            started_at_ms: None,
            last_progress_at_ms: Some(unix_time_ms_now()),
            attempt: None,
            last_attempt_reason: None,
            tokens: None,
            tool_calls: None,
            last_tool_name: None,
            last_tool_summary: None,
            prompt_preview: None,
        },
    }
}

#[derive(Clone)]
struct WorkflowAgentLiveObserver {
    progress_tx: Option<mpsc::UnboundedSender<String>>,
    tx: Option<mpsc::UnboundedSender<WorkflowProgressUpdate>>,
    state: Arc<tokio::sync::Mutex<WorkflowProgressUpdate>>,
    journal: Option<(WorkflowJournalWriter, String)>,
    metrics: Option<Arc<tokio::sync::Mutex<WorkflowRunMetrics>>>,
    call_index: u64,
}

impl WorkflowAgentLiveObserver {
    fn new_with_metrics(
        progress_tx: Option<mpsc::UnboundedSender<String>>,
        tx: Option<mpsc::UnboundedSender<WorkflowProgressUpdate>>,
        base: WorkflowProgressUpdate,
        journal: Option<(WorkflowJournalWriter, String)>,
        metrics: Option<Arc<tokio::sync::Mutex<WorkflowRunMetrics>>>,
        call_index: u64,
    ) -> Self {
        Self {
            progress_tx,
            tx,
            state: Arc::new(tokio::sync::Mutex::new(base)),
            journal,
            metrics,
            call_index,
        }
    }

    async fn publish_with<F>(&self, apply: F)
    where
        F: FnOnce(&mut WorkflowProgressUpdate),
    {
        let snapshot = {
            let mut state = self.state.lock().await;
            apply(&mut state);
            state.clone()
        };
        if let Some(metrics) = &self.metrics {
            metrics.lock().await.record_progress(
                self.call_index,
                snapshot.phase_index,
                snapshot.phase_title.clone(),
                snapshot.state.as_deref(),
                snapshot.error.as_deref(),
                snapshot.tokens,
                snapshot.tool_calls,
            );
        }
        emit_workflow_agent_snapshot(self.progress_tx.as_ref(), &snapshot);
        if let Some(tx) = &self.tx {
            let _ = tx.send(snapshot);
        }
    }

    async fn snapshot(&self) -> WorkflowProgressUpdate {
        self.state.lock().await.clone()
    }
}

#[async_trait]
impl platform_api::subagent_spawn::SubagentSpawnObserver for WorkflowAgentLiveObserver {
    async fn on_event(&self, event: platform_api::subagent_spawn::SubagentObservation) {
        match event {
            platform_api::subagent_spawn::SubagentObservation::Allocated {
                agent_id,
                agent_type,
                model,
                model_profile,
                ..
            } => {
                if let Some((journal, key)) = &self.journal {
                    journal.append_started(key, &agent_id.to_string()).await;
                }
                let now = unix_time_ms_now();
                self.publish_with(move |state| {
                    state.agent_id = Some(agent_id.to_string());
                    state.agent_type = Some(agent_type);
                    state.model = Some(platform_api::qualified_model_ref(
                        &model,
                        model_profile.as_deref(),
                    ));
                    state.state = Some("progress".to_string());
                    state.started_at_ms = Some(now);
                    state.last_progress_at_ms = Some(now);
                })
                .await;
            }
            platform_api::subagent_spawn::SubagentObservation::Progress {
                tool_use_count,
                token_count,
                ..
            } => {
                let now = unix_time_ms_now();
                self.publish_with(move |state| {
                    state.tokens = Some(token_count);
                    state.tool_calls = Some(u64::from(tool_use_count));
                    state.last_progress_at_ms = Some(now);
                })
                .await;
            }
            platform_api::subagent_spawn::SubagentObservation::Retry {
                attempt, reason, ..
            } => {
                let now = unix_time_ms_now();
                self.publish_with(move |state| {
                    state.state = Some("progress".to_string());
                    state.attempt = Some(attempt);
                    state.last_attempt_reason = Some(reason);
                    state.last_progress_at_ms = Some(now);
                })
                .await;
            }
            platform_api::subagent_spawn::SubagentObservation::Message { message, .. } => {
                let now = unix_time_ms_now();
                self.publish_with(move |state| {
                    state.last_progress_at_ms = Some(now);
                    if let protocol::ConversationMessage::Assistant { content, .. } = message {
                        for block in content {
                            if let protocol::ContentBlock::ToolUse { name, .. } = block {
                                state.last_tool_name = Some(name.clone());
                                state.last_tool_summary = Some(name);
                            }
                        }
                    }
                })
                .await;
            }
            platform_api::subagent_spawn::SubagentObservation::Completed {
                total_tool_use_count,
                total_duration_ms,
                usage,
                ..
            } => {
                let now = unix_time_ms_now();
                self.publish_with(move |state| {
                    state.state = Some("done".to_string());
                    state.tool_calls = Some(total_tool_use_count);
                    state.tokens = Some(
                        usage
                            .input_tokens
                            .saturating_add(usage.output_tokens)
                            .saturating_add(usage.cache_creation_input_tokens)
                            .saturating_add(usage.cache_read_input_tokens),
                    );
                    state.last_progress_at_ms = Some(now);
                })
                .await;
                if let Some(metrics) = &self.metrics {
                    metrics
                        .lock()
                        .await
                        .record_duration(self.call_index, total_duration_ms);
                }
            }
            platform_api::subagent_spawn::SubagentObservation::Failed { error, .. } => {
                let now = unix_time_ms_now();
                self.publish_with(move |state| {
                    state.state = Some("error".to_string());
                    state.error = Some(error);
                    state.last_progress_at_ms = Some(now);
                })
                .await;
            }
            platform_api::subagent_spawn::SubagentObservation::Killed { .. } => {
                let now = unix_time_ms_now();
                self.publish_with(move |state| {
                    state.state = Some("error".to_string());
                    state.error = Some("subagent was cancelled".to_string());
                    state.last_progress_at_ms = Some(now);
                })
                .await;
            }
        }
    }
}

/// Backs the script's `budget` global: a fixed `total` (the turn's token target)
/// and `spent()` = output tokens spent SINCE THE CURRENT TURN STARTED — the
/// shared cumulative pool minus the turn-start baseline (`baseline`). This is
/// claude-code `getTurnSpent()` = `rT() - R` where `R = xtr` is the cumulative
/// output at turn start (binary @192177594/@202008079): a turn-relative delta,
/// NOT the absolute session pool. `baseline` is snapshotted at workflow start.
struct OwnSpendBudget {
    total: Option<u64>,
    spent: Arc<std::sync::atomic::AtomicU64>,
    /// Cumulative output at the start of the turn this workflow was spawned in.
    baseline: u64,
    output_scope: Option<platform_api::WorkflowOutputScope>,
}
impl OwnSpendBudget {
    fn turn_spent(&self) -> u64 {
        if let Some(scope) = &self.output_scope {
            return scope.spent();
        }
        self.spent
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(self.baseline)
    }
}
impl workflow::WorkflowBudgetSource for OwnSpendBudget {
    fn total(&self) -> Option<u64> {
        self.total
    }
    fn spent(&self) -> u64 {
        self.turn_spent()
    }
}

/// Known output across the entire agent run, including separately reported
/// reasoning. Older spawners omit cumulative usage, so retain their final
/// report as a fallback. A failed run can still have already-billed output.
fn known_workflow_agent_output(
    result: &SubagentResult,
) -> Result<Option<u64>, platform_api::BudgetError> {
    let usage = match result {
        SubagentResult::Completed {
            usage,
            cumulative_usage,
            ..
        } => {
            if *cumulative_usage == platform_api::SubagentUsage::default() {
                usage
            } else {
                cumulative_usage
            }
        }
        SubagentResult::Failed { usage, .. } => usage,
        SubagentResult::Killed { .. } => return Ok(None),
    };
    usage
        .output_tokens
        .checked_add(usage.reasoning_output_tokens)
        .map(Some)
        .ok_or_else(|| platform_api::BudgetError::Internal("workflow agent output overflow".into()))
}

#[cfg(test)]
mod captured_output_tests {
    use super::*;
    use platform_api::{
        BudgetError, WorkflowOutputAccount, WorkflowOutputEventId, WorkflowOutputScope,
    };
    use std::sync::atomic::Ordering;

    struct Account {
        session: protocol::SessionId,
        generation: protocol::MessageId,
        spent: AtomicU64,
    }
    impl WorkflowOutputAccount for Account {
        fn session_id(&self) -> protocol::SessionId {
            self.session
        }
        fn generation_id(&self) -> protocol::MessageId {
            self.generation
        }
        fn spent(&self) -> u64 {
            self.spent.load(Ordering::Relaxed)
        }
        fn record_legacy(&self, _: WorkflowOutputEventId, tokens: u64) -> Result<(), BudgetError> {
            self.spent.fetch_add(tokens, Ordering::Relaxed);
            Ok(())
        }
    }
    fn scope(session: protocol::SessionId) -> WorkflowOutputScope {
        WorkflowOutputScope::new(Arc::new(Account {
            session,
            generation: protocol::MessageId::new(),
            spent: AtomicU64::new(0),
        }))
    }
    fn reader(scope: WorkflowOutputScope) -> OwnSpendBudget {
        OwnSpendBudget {
            total: Some(100),
            spent: Arc::new(AtomicU64::new(900)),
            baseline: 0,
            output_scope: Some(scope),
        }
    }

    struct ExecutionProbe {
        session: protocol::SessionId,
        generation: protocol::MessageId,
        events: std::sync::Mutex<HashMap<WorkflowOutputEventId, u64>>,
        calls: AtomicU64,
        result: Option<SubagentResult>,
    }
    impl WorkflowOutputAccount for ExecutionProbe {
        fn session_id(&self) -> protocol::SessionId {
            self.session
        }
        fn generation_id(&self) -> protocol::MessageId {
            self.generation
        }
        fn spent(&self) -> u64 {
            self.events.lock().unwrap().values().sum()
        }
        fn record_legacy(
            &self,
            event: WorkflowOutputEventId,
            tokens: u64,
        ) -> Result<(), BudgetError> {
            let mut events = self.events.lock().unwrap();
            if let Some(old) = events.get(&event) {
                assert_eq!(*old, tokens);
            } else {
                events.insert(event, tokens);
            }
            Ok(())
        }
    }
    #[async_trait]
    impl SubagentSpawner for ExecutionProbe {
        async fn spawn(
            &self,
            _: SubagentSpawnRequest,
            _: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(result) = &self.result {
                return Ok(result.clone());
            }
            Ok(SubagentResult::Completed {
                agent_id: protocol::AgentId::new(),
                content: Value::String("answer".into()),
                usage: platform_api::SubagentUsage {
                    output_tokens: 7,
                    ..Default::default()
                },
                total_tool_use_count: 0,
                total_duration_ms: 0,
                total_tokens: 7,
                assistant_message_count: 0,
                response_char_count: 0,
                last_request_id: None,
                cumulative_usage: platform_api::SubagentUsage::default(),
                usage_complete: true,
            })
        }
    }
    #[async_trait]
    impl ToolInvoker for ExecutionProbe {
        async fn invoke(
            &self,
            _: &str,
            _: Value,
            _: SubagentInvocationContext,
        ) -> Result<Value, ToolInvokerError> {
            Ok(Value::Null)
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    #[async_trait]
    impl BudgetEnforcerHandle for ExecutionProbe {
        async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    #[tokio::test]
    async fn captured_output_resume_cache_misses_have_distinct_execution_events() {
        let probe = Arc::new(ExecutionProbe {
            session: protocol::SessionId::new(),
            generation: protocol::MessageId::new(),
            events: std::sync::Mutex::new(HashMap::new()),
            calls: AtomicU64::new(0),
            result: None,
        });
        let scope = WorkflowOutputScope::new(probe.clone());
        let journal = Arc::new(std::sync::Mutex::new(HashMap::new()));
        for script in [
            "return await agent('first prompt');",
            "return await agent('changed prompt');",
            "return await agent('changed prompt');",
        ] {
            run_workflow_script_with_live_updates_recorded(
                script,
                DEFAULT_WORKFLOW_SUBAGENT,
                "workflow",
                probe.clone(),
                probe.clone(),
                probe.clone(),
                None,
                None,
                Some(journal.clone()),
                None,
                Some(100),
                None,
                0,
                NestedConfig::default(),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(AnalyticsBus::new()),
                None,
                None,
                None,
                Some(scope.clone()),
            )
            .await
            .unwrap();
        }
        assert_eq!(
            probe.calls.load(Ordering::Relaxed),
            2,
            "both cache misses actually spawned"
        );
        assert_eq!(
            scope.spent(),
            14,
            "resume must not deduplicate a new physical execution"
        );
        assert_eq!(probe.events.lock().unwrap().len(), 2);
    }

    fn completed_usage(
        output: u64,
        reasoning: u64,
        cumulative: platform_api::SubagentUsage,
    ) -> SubagentResult {
        SubagentResult::Completed {
            agent_id: protocol::AgentId::new(),
            content: Value::String("answer".into()),
            usage: platform_api::SubagentUsage {
                output_tokens: output,
                reasoning_output_tokens: reasoning,
                ..Default::default()
            },
            total_tool_use_count: 0,
            total_duration_ms: 0,
            total_tokens: output,
            assistant_message_count: 0,
            response_char_count: 0,
            last_request_id: None,
            cumulative_usage: cumulative,
            usage_complete: false,
        }
    }

    #[tokio::test]
    async fn captured_output_records_all_known_rounds_reasoning_and_failed_spend() {
        let cases = [
            (
                completed_usage(
                    7,
                    3,
                    platform_api::SubagentUsage {
                        output_tokens: 21,
                        reasoning_output_tokens: 9,
                        ..Default::default()
                    },
                ),
                30,
            ),
            (
                completed_usage(
                    7,
                    3,
                    platform_api::SubagentUsage {
                        reasoning_output_tokens: 11,
                        ..Default::default()
                    },
                ),
                11,
            ),
            (
                completed_usage(7, 3, platform_api::SubagentUsage::default()),
                10,
            ),
            (
                SubagentResult::Failed {
                    agent_id: protocol::AgentId::new(),
                    reason: "after paid round".into(),
                    usage: platform_api::SubagentUsage {
                        output_tokens: 12,
                        reasoning_output_tokens: 5,
                        ..Default::default()
                    },
                },
                17,
            ),
        ];
        for (result, expected) in cases {
            for scoped in [true, false] {
                let legacy_expected = match &result {
                    SubagentResult::Completed { usage, .. } => usage.output_tokens,
                    _ => 0,
                };
                let probe = Arc::new(ExecutionProbe {
                    session: protocol::SessionId::new(),
                    generation: protocol::MessageId::new(),
                    events: std::sync::Mutex::new(HashMap::new()),
                    calls: AtomicU64::new(0),
                    result: Some(result.clone()),
                });
                let scope = WorkflowOutputScope::new(probe.clone());
                let legacy_pool = Arc::new(AtomicU64::new(0));
                let _ = run_workflow_script_with_live_updates_recorded(
                    "try { return await agent('probe'); } catch (error) { return 'failed'; }",
                    DEFAULT_WORKFLOW_SUBAGENT,
                    "workflow",
                    probe.clone(),
                    probe.clone(),
                    probe.clone(),
                    None,
                    None,
                    None,
                    None,
                    Some(100),
                    Some(legacy_pool.clone()),
                    0,
                    NestedConfig::default(),
                    Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    Arc::new(AnalyticsBus::new()),
                    None,
                    None,
                    None,
                    scoped.then(|| scope.clone()),
                )
                .await
                .unwrap();
                assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
                assert_eq!(scope.spent(), if scoped { expected } else { 0 });
                assert_eq!(
                    legacy_pool.load(Ordering::Relaxed),
                    if scoped { 0 } else { legacy_expected }
                );
            }
        }
    }

    #[test]
    fn captured_output_rejects_overflow_without_inventing_killed_usage() {
        let overflow = completed_usage(u64::MAX, 1, platform_api::SubagentUsage::default());
        assert!(known_workflow_agent_output(&overflow).is_err());
        assert_eq!(
            known_workflow_agent_output(&SubagentResult::Killed {
                agent_id: protocol::AgentId::new()
            })
            .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn captured_output_caught_overflow_cannot_spawn_again() {
        let probe = Arc::new(ExecutionProbe {
            session: protocol::SessionId::new(),
            generation: protocol::MessageId::new(),
            events: std::sync::Mutex::new(HashMap::new()),
            calls: AtomicU64::new(0),
            result: Some(completed_usage(
                u64::MAX,
                1,
                platform_api::SubagentUsage::default(),
            )),
        });
        let scope = WorkflowOutputScope::new(probe.clone());
        run_workflow_script_with_live_updates_recorded(
            "try { await agent('overflow'); } catch (e) {} try { await agent('must not dispatch'); } catch (e) {} return 'done';",
            DEFAULT_WORKFLOW_SUBAGENT, "workflow", probe.clone(), probe.clone(), probe.clone(),
            None, None, None, None, Some(100), None, 0, NestedConfig::default(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(AnalyticsBus::new()), None, None, None, Some(scope.clone()),
        ).await.unwrap();
        assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
        assert!(
            probe.events.lock().unwrap().is_empty(),
            "overflow must not create a fabricated token count"
        );
    }

    #[test]
    fn captured_output_workflows_share_main_and_workflow_spend() {
        let scope = scope(protocol::SessionId::new());
        let first = reader(scope.clone());
        let second = reader(scope.clone());
        scope
            .record_legacy(
                WorkflowOutputEventId::MainResponse(protocol::MessageId::new()),
                7,
            )
            .unwrap();
        scope
            .record_legacy(
                WorkflowOutputEventId::WorkflowAgent {
                    run_id: "run".into(),
                    call_index: 1,
                },
                11,
            )
            .unwrap();
        assert_eq!(first.turn_spent(), 18);
        assert_eq!(second.turn_spent(), 18);
    }

    #[test]
    fn captured_output_late_parent_and_nested_reads_keep_original_generation() {
        let session = protocol::SessionId::new();
        let old = scope(session);
        let parent = reader(old.clone());
        let nested = reader(old.clone());
        let current = reader(scope(session));
        old.record_legacy(
            WorkflowOutputEventId::WorkflowAgent {
                run_id: "wf_test".into(),
                call_index: 0,
            },
            23,
        )
        .unwrap();
        assert_eq!(parent.turn_spent(), 23);
        assert_eq!(nested.turn_spent(), 23);
        assert_eq!(current.turn_spent(), 0);
    }
}

/// Nesting config for a workflow run: whether top-level `workflow()` is allowed
/// (a nested run sets `false`, enforcing claude-code's one-level limit), the
/// `args` global (JSON string), and a filesystem to resolve a nested
/// `workflow({scriptPath})` / `workflow(name)`.
#[derive(Default, Clone)]
pub struct NestedConfig {
    /// `true` for the top-level run; `false` inside a nested workflow.
    pub allow_nested: bool,
    /// The `args` global value as a JSON string. `None` ⇒ `undefined`.
    pub args: Option<String>,
    /// Filesystem for resolving `workflow({scriptPath})` / `workflow(name)`.
    pub fs: Option<Arc<dyn FileSystem>>,
    /// Live plugin-workflow registry (§14) consulted by `workflow(name)`
    /// after the project/user saved-workflow directories have missed.
    /// `None` ⇒ only built-in/project/user workflows resolve, exactly
    /// today's behavior.
    pub plugin_workflows: Option<Arc<workflow::PluginWorkflowRegistry>>,
    /// Session that owns this workflow run, kept even if the active
    /// router/model changes while a background workflow is still running.
    pub session_uuid: Option<String>,
}

/// Per-call plan for one batch: decided sequentially in Phase A (prefix-cache
/// cursor), executed concurrently in Phase B.
enum Plan {
    /// `__wf_resolve` — resolve a nested workflow reference to its source.
    Resolve(Value),
    /// Replay a journaled result (prefix hit).
    Cached {
        /// The cached result string.
        result: String,
        /// 0-based agent call ordinal.
        call_index: u64,
        /// The agent's label: `opts.label ?? prompt.slice(0, 60)`.
        label: String,
        /// Phase info at dispatch time (extracted from `__wf_phase`).
        phase_index: Option<u32>,
        /// Phase title at dispatch time.
        phase_title: Option<String>,
    },
    /// Spawn a real subagent; journal the result under `key` (when present).
    Live {
        key: Option<String>,
        prompt: String,
        opts_json: String,
        /// 0-based agent call ordinal.
        call_index: u64,
        /// The agent's label: `opts.label ?? prompt.slice(0, 60)`.
        label: String,
        /// Phase info at dispatch time (extracted from `__wf_phase`).
        phase_index: Option<u32>,
        /// Phase title at dispatch time.
        phase_title: Option<String>,
    },
}

/// Context fields for `tengu_workflow_phase_completed` events (oracle §7).
/// these are not available inside `run_workflow_script` itself (the run_id is
/// minted in the outer worker closure), so the caller threads them in as a bundle.
/// When `None`, phase events still fire but omit the context fields.
#[derive(Debug, Clone)]
pub struct PhaseTelemetryCtx {
    /// The `wf_…` run id for this workflow invocation.
    pub run_id: String,
    /// Resolved source category: `"built-in"`, `"projectSettings"`,
    /// `"userSettings"`, `"plugin"`, `"scriptPath"`, or `"inline"`.
    pub workflow_source: Option<String>,
    /// Whether the resolved script is byte-identical to the bundled definition.
    pub script_is_verbatim_builtin: Option<bool>,
    /// `meta.name` from the workflow script.
    pub workflow_name: Option<String>,
    /// How the workflow was invoked: `"scriptPath"` | `"named"` | `"inline"`.
    ///
    /// Oracle §7: `tengu_workflow_phase_completed` is gated on `p.source === "built-in"`
    /// and a verbatim bundled script. Inline/custom named scripts and arbitrary
    /// `scriptPath` invocations do not emit this event.
    pub invocation_mode: Option<String>,
}

async fn emit_phase_completed(
    bus: &AnalyticsBus,
    phase_telemetry_ctx: Option<&PhaseTelemetryCtx>,
    metrics: &WorkflowRunMetrics,
) {
    let is_builtin_source = phase_telemetry_ctx.is_some_and(|ctx| {
        telemetry_is_verbatim_builtin(
            ctx.workflow_source.as_deref(),
            ctx.script_is_verbatim_builtin,
        )
    });
    if !is_builtin_source {
        return;
    }
    for (phase_index, phase) in metrics.phase_metrics() {
        let mut md: LogEventMetadata = HashMap::new();
        if let Some(ctx) = phase_telemetry_ctx {
            md.insert(
                "workflow_run_id".to_string(),
                AnalyticsValue::String(ctx.run_id.clone()),
            );
            if let Some(ref src) = ctx.workflow_source {
                md.insert(
                    "workflow_source".to_string(),
                    AnalyticsValue::String(src.clone()),
                );
            }
            if let Some(ref name) = ctx.workflow_name {
                md.insert(
                    "workflow_name".to_string(),
                    AnalyticsValue::String(name.clone()),
                );
            }
        }
        md.insert(
            "phase_index".to_string(),
            AnalyticsValue::Int(i64::from(phase_index)),
        );
        md.insert(
            "phase_title".to_string(),
            AnalyticsValue::String(phase.title),
        );
        md.insert(
            "phase_tokens".to_string(),
            AnalyticsValue::Int(phase.tokens as i64),
        );
        md.insert(
            "phase_tool_calls".to_string(),
            AnalyticsValue::Int(phase.tool_calls as i64),
        );
        md.insert(
            "phase_agent_duration_ms".to_string(),
            AnalyticsValue::Int(phase.duration_ms as i64),
        );
        md.insert(
            "phase_agent_count".to_string(),
            AnalyticsValue::Int(phase.agent_count as i64),
        );
        md.insert(
            "phase_error_count".to_string(),
            AnalyticsValue::Int(phase.error_count as i64),
        );
        md.insert(
            "phase_skip_count".to_string(),
            AnalyticsValue::Int(phase.skip_count as i64),
        );
        bus.log_event(telemetry::tengu::workflow::PHASE_COMPLETED, md)
            .await;
    }
}

/// Run a workflow `script` to completion, spawning each `agent()` call as a real
/// subagent of type `subagent_type` via `spawner`. Returns the script's
/// [`workflow::RunOutcome`] (its `phase()`/`log()` progress + return value) or a
/// [`workflow::WorkflowError`].
///
/// `tool_invoker` and `budget` are the parent's inheritance `Arc`s; the same
/// `Arc`s (cloned handle, identical inner) are handed to every child so the
/// recursion lock and budget aggregate across the whole agent tree.
///
/// `shared_pool` backs the script's `budget.spent()`: when `Some`, it is the
/// session-wide pool the main loop also feeds (every successful main-loop API
/// response adds its output tokens), so `spent()` reads main loop + every
/// workflow — claude-code's shared pool. When `None` (tests / no orchestrator),
/// the run uses a fresh private pool counting only its own subagent output.
/// The plugin namespace the running workflow belongs to, or `None` when it is
/// not a plugin's workflow.
///
/// `workflow_id` is plugin-qualified (`<plugin>:<workflow>`) only when the Host
/// launched the run BY NAME through the plugin registry. Two production paths
/// lose that qualification and hand this function the script's bare
/// `meta.name` instead:
///   * resume — `runtime/src/mobile/host.rs` relaunches a paused/adopted
///     run with `WorkflowLaunchSpec { script_path: Some(..), name: None, .. }`,
///     so `workflow_support.rs`'s `namespaced_plugin_workflow` is `None` and
///     the id falls back to `meta.name`;
///   * desktop — `runtime/src/desktop/mod.rs` derives `workflow_id` from
///     `meta.name` FIRST and only falls back to `spec.name`.
/// In both cases the id carries no `:`, so recover the namespace from the
/// plugin workflow registry itself: a bare id that exactly one plugin
/// registers as `<plugin>:<id>` belongs to that plugin. Ambiguity (two plugins
/// registering the same bare workflow name) resolves to `None` rather than
/// guessing an owner.
///
/// Only the FIRST `:` splits — a plugin's workflow (or agent) name can itself
/// contain `:` when it comes from a nested directory inside the plugin.
///
/// This grants no new reach: the caller only uses the namespace to look up a
/// name that must ALREADY be in the spawner's agent listing, and any script
/// could always have written that qualified name out in full.
fn plugin_namespace_for_workflow(
    workflow_id: &str,
    plugin_workflows: Option<&workflow::PluginWorkflowRegistry>,
) -> Option<String> {
    if let Some((namespace, _)) = workflow_id.split_once(':') {
        return (!namespace.is_empty()).then(|| namespace.to_string());
    }
    if workflow_id.is_empty() {
        return None;
    }
    let registry = plugin_workflows?;
    let mut found: Option<String> = None;
    for name in registry.names() {
        let Some((namespace, rest)) = name.split_once(':') else {
            continue;
        };
        if namespace.is_empty() || rest != workflow_id {
            continue;
        }
        if found.as_deref().is_some_and(|already| already != namespace) {
            return None;
        }
        found = Some(namespace.to_string());
    }
    found
}

// Argument-defaulting wrappers over the recorded entrypoint. Production has
// no caller: every host goes through `..._recorded`. They exist so a test can
// omit the recorder and telemetry arguments, so they are compiled only
// for tests rather than shipped as public API.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_workflow_script(
    script: &str,
    subagent_type: &str,
    // The caller-supplied `workflow_id` this run was launched under (e.g. a
    // plugin-qualified `<plugin>:<workflow>`, or unqualified for a saved/inline
    // workflow). Used only to resolve a bare `agentType` against the plugin's
    // own namespace when the exact name is not in the agent listing -- see the
    // `agentType` pre-check in the live-agent dispatch loop below.
    workflow_id: &str,
    spawner: Arc<dyn SubagentSpawner>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    progress_tx: Option<mpsc::UnboundedSender<String>>,
    journal: Option<Arc<std::sync::Mutex<HashMap<String, String>>>>,
    token_budget_total: Option<u64>,
    shared_pool: Option<Arc<AtomicU64>>,
    turn_start_baseline: u64,
    nested: NestedConfig,
    // Cooperative-cancel flag (claude-code `abortController`). `Task::kill` flips
    // it; the embedded engine's interrupt handler then aborts the script thread,
    // so a runaway pure-JS loop is stopped instead of leaking the OS thread.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    bus: Arc<AnalyticsBus>,
    // Legacy compatibility seam. Current callers read the run metrics snapshot;
    // this argument is accepted but intentionally not used as the source of truth.
    agent_count_out: Option<Arc<AtomicU64>>,
    // Optional fields for tengu_workflow_phase_completed (oracle §7):
    // `workflow_run_id`, `workflow_source`, `workflow_name`. When None, the
    // phase_completed events still fire but omit these optional context fields.
    phase_telemetry_ctx: Option<PhaseTelemetryCtx>,
) -> Result<workflow::RunOutcome, workflow::WorkflowError> {
    run_workflow_script_with_live_updates(
        script,
        subagent_type,
        workflow_id,
        spawner,
        tool_invoker,
        budget,
        progress_tx,
        None,
        journal,
        None,
        token_budget_total,
        shared_pool,
        turn_start_baseline,
        nested,
        cancel,
        bus,
        agent_count_out,
        phase_telemetry_ctx,
        None,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_workflow_script_with_live_updates(
    script: &str,
    subagent_type: &str,
    workflow_id: &str,
    spawner: Arc<dyn SubagentSpawner>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    progress_tx: Option<mpsc::UnboundedSender<String>>,
    live_progress_tx: Option<mpsc::UnboundedSender<WorkflowProgressUpdate>>,
    journal: Option<Arc<std::sync::Mutex<HashMap<String, String>>>>,
    journal_writer: Option<WorkflowJournalWriter>,
    token_budget_total: Option<u64>,
    shared_pool: Option<Arc<AtomicU64>>,
    turn_start_baseline: u64,
    nested: NestedConfig,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    bus: Arc<AnalyticsBus>,
    agent_count_out: Option<Arc<AtomicU64>>,
    phase_telemetry_ctx: Option<PhaseTelemetryCtx>,
    workflow_metrics_out: Option<Arc<tokio::sync::Mutex<WorkflowRunMetrics>>>,
) -> Result<workflow::RunOutcome, workflow::WorkflowError> {
    run_workflow_script_with_live_updates_full(
        script,
        subagent_type,
        workflow_id,
        spawner,
        tool_invoker,
        budget,
        progress_tx,
        live_progress_tx,
        journal,
        journal_writer,
        token_budget_total,
        shared_pool,
        turn_start_baseline,
        nested,
        cancel,
        bus,
        agent_count_out,
        phase_telemetry_ctx,
        workflow_metrics_out,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_workflow_script_with_live_updates_full(
    script: &str,
    subagent_type: &str,
    workflow_id: &str,
    spawner: Arc<dyn SubagentSpawner>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    progress_tx: Option<mpsc::UnboundedSender<String>>,
    live_progress_tx: Option<mpsc::UnboundedSender<WorkflowProgressUpdate>>,
    journal: Option<Arc<std::sync::Mutex<HashMap<String, String>>>>,
    journal_writer: Option<WorkflowJournalWriter>,
    token_budget_total: Option<u64>,
    shared_pool: Option<Arc<AtomicU64>>,
    turn_start_baseline: u64,
    nested: NestedConfig,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    bus: Arc<AnalyticsBus>,
    agent_count_out: Option<Arc<AtomicU64>>,
    phase_telemetry_ctx: Option<PhaseTelemetryCtx>,
    workflow_metrics_out: Option<Arc<tokio::sync::Mutex<WorkflowRunMetrics>>>,
) -> Result<workflow::RunOutcome, workflow::WorkflowError> {
    run_workflow_script_with_live_updates_recorded(
        script,
        subagent_type,
        workflow_id,
        spawner,
        tool_invoker,
        budget,
        progress_tx,
        live_progress_tx,
        journal,
        journal_writer,
        token_budget_total,
        shared_pool,
        turn_start_baseline,
        nested,
        cancel,
        bus,
        agent_count_out,
        phase_telemetry_ctx,
        workflow_metrics_out,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_workflow_script_with_live_updates_recorded(
    script: &str,
    subagent_type: &str,
    workflow_id: &str,
    spawner: Arc<dyn SubagentSpawner>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    progress_tx: Option<mpsc::UnboundedSender<String>>,
    live_progress_tx: Option<mpsc::UnboundedSender<WorkflowProgressUpdate>>,
    journal: Option<Arc<std::sync::Mutex<HashMap<String, String>>>>,
    journal_writer: Option<WorkflowJournalWriter>,
    token_budget_total: Option<u64>,
    shared_pool: Option<Arc<AtomicU64>>,
    turn_start_baseline: u64,
    nested: NestedConfig,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    bus: Arc<AnalyticsBus>,
    agent_count_out: Option<Arc<AtomicU64>>,
    phase_telemetry_ctx: Option<PhaseTelemetryCtx>,
    workflow_metrics_out: Option<Arc<tokio::sync::Mutex<WorkflowRunMetrics>>>,
    output_scope: Option<platform_api::WorkflowOutputScope>,
) -> Result<workflow::RunOutcome, workflow::WorkflowError> {
    let NestedConfig {
        allow_nested,
        args: nested_args,
        fs: nested_fs,
        plugin_workflows: nested_plugin_workflows,
        session_uuid: workflow_session_uuid,
    } = nested;
    use std::sync::atomic::Ordering;
    // Resume reuses the persisted run/journal identity, but a real script
    // execution can issue new calls at the same call indices. Its accounting
    // namespace must therefore be fresh; nested calls share this execution.
    let output_execution_id = protocol::MessageId::new().to_string();
    // Script try/catch must not turn an accounting failure into permission
    // for another paid call, including later calls in the same batch.
    let output_accounting_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let emit_phase_telemetry_here = workflow_metrics_out.is_none();
    let workflow_metrics = workflow_metrics_out
        .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(WorkflowRunMetrics::default())));

    // The script's `budget`: `total` is the turn's target; `spent()` reads the
    // shared pool — main loop (fed by the orchestrator per response) plus every
    // workflow (the worker adds each fresh subagent's output tokens; replayed
    // journaled agents cost nothing, as in claude-code). Without a shared pool
    // (tests), a private counter tracks this run's own subagent output only.
    let spent = shared_pool.unwrap_or_else(|| Arc::new(AtomicU64::new(0)));
    let budget_source: Arc<dyn workflow::WorkflowBudgetSource> = Arc::new(OwnSpendBudget {
        total: token_budget_total,
        spent: spent.clone(),
        baseline: turn_start_baseline,
        output_scope: output_scope.clone(),
    });
    // Request channel: each in-flight batch is (calls, reply-sender), where a
    // call is (prompt, opts_json). A buffer of one suffices — the runner blocks
    // on its reply before sending the next.
    let (req_tx, mut req_rx) =
        mpsc::channel::<(Vec<(String, String)>, oneshot::Sender<Vec<String>>)>(1);
    let (outcome_tx, outcome_rx) =
        oneshot::channel::<Result<workflow::RunOutcome, workflow::WorkflowError>>();
    let script_owned = script.to_string();
    // Clone progress_tx for the async worker loop (agent lifecycle events). The
    // script thread owns the original for phase()/log() events; the async worker
    // uses this clone to emit workflow_agent start/done/error/cached events.
    let progress_tx_for_worker = progress_tx.clone();
    let live_progress_tx_for_worker = live_progress_tx.clone();
    let workflow_metrics_for_script = workflow_metrics.clone();

    // The script runs synchronously on its own OS thread; its batch runner is
    // plain sync code, so `blocking_send`/`blocking_recv` are safe here (this is
    // not a runtime worker thread).
    std::thread::Builder::new()
        .name("workflow-script".into())
        .spawn(move || {
            let runner = move |prompts: &[String], opts_json: &[String]| -> Vec<String> {
                let (reply_tx, reply_rx) = oneshot::channel();
                let calls: Vec<(String, String)> = prompts
                    .iter()
                    .cloned()
                    .zip(opts_json.iter().cloned())
                    .collect();
                if req_tx.blocking_send((calls, reply_tx)).is_err() {
                    return Vec::new();
                }
                reply_rx.blocking_recv().unwrap_or_default()
            };
            // `phase()`/`log()` fire this live; an unbounded `send` is non-blocking
            // and needs no runtime, so it is safe from the script thread. The host
            // drains `progress_tx` concurrently (e.g. spools to the task output).
            let on_progress = move |p: &workflow::Progress| {
                if let workflow::Progress::Phase { index, title } = p {
                    // The callback runs on the dedicated script thread, so a
                    // blocking lock cannot stall the async runtime. Record
                    // phase-only workflows independently of agent metrics.
                    workflow_metrics_for_script
                        .blocking_lock()
                        .record_phase(*index, title.clone());
                }
                if let Some(tx) = &progress_tx {
                    let _ = tx.send(format_progress(p));
                }
                if let Some(tx) = &live_progress_tx {
                    let _ = tx.send(workflow_progress_update(p));
                }
            };
            let outcome = workflow::run_with_progress(
                &script_owned,
                runner,
                on_progress,
                Some(budget_source),
                allow_nested,
                nested_args,
                Some(cancel),
            );
            let _ = outcome_tx.send(outcome);
        })
        .expect("spawn workflow-script thread");

    let cap = concurrency_cap();
    // The current Claude runtime derives the cap/telemetry count from every
    // real `agent()` call, including journal-cache hits. Keep the old optional
    // counter seam accepted for callers, but do not use it as the source of
    // truth for this run. `__wf_resolve` calls are not real agents.
    let _ = agent_count_out;
    // Per-run agent call ordinal counter (oracle §8: the `index` field on
    // `workflow_agent` events is monotonically incrementing per call, including
    // cached replay calls; `__wf_resolve` calls do NOT count).
    let call_index_counter = Arc::new(AtomicU64::new(0));
    // Use the pre-cloned progress_tx for the async worker (agent lifecycle events).
    let worker_progress_tx = progress_tx_for_worker;
    let worker_live_progress_tx = live_progress_tx_for_worker;
    // PREFIX resume cursor (claude-code `m` + gone-live flag `f`): the journal is
    // a longest-unchanged-prefix cache. `running_key` chains each real agent()
    // call into the previous key (so any change cascades to all later keys), and
    // once a lookup MISSES we go live (`gone_live`) for every later call — never
    // replaying a stale result out of order. Both advance in agent()-CALL order,
    // which the worker sees batch-by-batch (this loop is sequential) and, within
    // a batch, in the queue's call order. `__wf_resolve` (nested-workflow source)
    // calls are not real agents: they neither advance the chain nor touch the
    // journal.
    let mut running_key = String::new();
    let mut gone_live = false;

    // Async worker: answer each batch. Phase A decides cached-vs-live per call in
    // order (advancing the prefix cursor); Phase B runs the plans concurrently
    // (bounded, order-preserving). The loop ends when the runner's sender is
    // dropped — i.e. when `workflow::run` returns.
    let mut agent_open = true;
    loop {
        let maybe_work = tokio::select! {
            maybe = req_rx.recv(), if agent_open => {
                match maybe {
                    Some((calls, reply)) => Some(WorkflowBridgeRequest::AgentBatch(calls, reply)),
                    None => {
                        agent_open = false;
                        None
                    }
                }
            }
            else => None,
        };
        let Some(work) = maybe_work else {
            if !agent_open {
                break;
            }
            continue;
        };
        if output_accounting_failed.load(Ordering::Acquire) {
            let error = wf_throw("workflow output accounting failed; further dispatch is disabled");
            match work {
                WorkflowBridgeRequest::AgentBatch(calls, reply) => {
                    let _ = reply.send(vec![error; calls.len()]);
                }
            }
            continue;
        }
        match work {
            WorkflowBridgeRequest::AgentBatch(calls, reply) => {
                // Phase A — sequential, in call order: prefix-cache decision per call.
                let mut plans: Vec<Plan> = Vec::with_capacity(calls.len());
                for (prompt, opts_json) in calls {
                    let mut opts: Value = serde_json::from_str(&opts_json).unwrap_or(Value::Null);
                    if let Some(spec_json) = opts.get("__wf_resolve").and_then(Value::as_str) {
                        let spec: Value = serde_json::from_str(spec_json).unwrap_or(Value::Null);
                        plans.push(Plan::Resolve(spec));
                        continue;
                    }
                    let (phase_index, phase_title) = if let Some(ph) =
                        opts.as_object_mut().and_then(|o| o.remove("__wf_phase"))
                    {
                        let idx = ph.get("index").and_then(Value::as_u64).map(|v| v as u32);
                        let title = ph.get("title").and_then(Value::as_str).map(str::to_string);
                        (idx, title)
                    } else {
                        (None, None)
                    };
                    let clean_opts_json =
                        serde_json::to_string(&opts).unwrap_or_else(|_| opts_json.clone());
                    let label = opts
                        .get("label")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            let chars: Vec<char> = prompt.chars().take(60).collect();
                            chars.iter().collect()
                        });
                    let normalized_opts = normalize_opts_for_chain_key(&opts);
                    let key = chain_key(&running_key, &prompt, &normalized_opts);
                    running_key.clone_from(&key);
                    let call_index =
                        call_index_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if !gone_live {
                        let cached = journal
                            .as_ref()
                            .and_then(|j| j.lock().unwrap().get(&key).cloned());
                        if let Some(cached) = cached {
                            plans.push(Plan::Cached {
                                result: cached,
                                call_index,
                                label,
                                phase_index,
                                phase_title,
                            });
                            continue;
                        }
                        gone_live = true;
                    }
                    let journaled_key = journal.as_ref().map(|_| key);
                    plans.push(Plan::Live {
                        key: journaled_key,
                        prompt,
                        opts_json: clean_opts_json,
                        call_index,
                        label,
                        phase_index,
                        phase_title,
                    });
                }

                let results: Vec<String> = futures::stream::iter(plans.into_iter().map(|plan| {
                    let spawner = spawner.clone();
                    let tool_invoker = tool_invoker.clone();
                    let budget = budget.clone();
                    let subagent_type = subagent_type.to_string();
                    let workflow_id = workflow_id.to_string();
                    let journal = journal.clone();
                    let journal_writer = journal_writer.clone();
                    let spent = spent.clone();
                    let output_scope = output_scope.clone();
                    let output_run_id = output_execution_id.clone();
                    let output_accounting_failed = output_accounting_failed.clone();
                    let nested_fs = nested_fs.clone();
                    let nested_plugin_workflows = nested_plugin_workflows.clone();
                    let budget_total = token_budget_total;
                    let baseline = turn_start_baseline;
                    let bus_call = bus.clone();
                    let ptx = worker_progress_tx.clone();
                    let live_tx = worker_live_progress_tx.clone();
                    let workflow_metrics = workflow_metrics.clone();
                    let workflow_session_uuid = workflow_session_uuid.clone();
                    async move {
                        if output_accounting_failed.load(Ordering::Acquire) {
                            return wf_throw("workflow output accounting failed; further dispatch is disabled");
                        }
                        if !matches!(&plan, Plan::Resolve(_)) {
                            if let Some(total) = budget_total.filter(|&t| t > 0) {
                                let turn_spent = output_scope.as_ref().map_or_else(
                                    || spent.load(Ordering::Relaxed).saturating_sub(baseline),
                                    platform_api::WorkflowOutputScope::spent,
                                );
                                if turn_spent >= total {
                                    let should_emit = {
                                        let mut metrics = workflow_metrics.lock().await;
                                        if metrics.budget_telemetry_emitted {
                                            false
                                        } else {
                                            metrics.budget_telemetry_emitted = true;
                                            true
                                        }
                                    };
                                    if should_emit {
                                        let mut md: LogEventMetadata = HashMap::new();
                                        md.insert(
                                            "spent".to_string(),
                                            AnalyticsValue::Int(turn_spent as i64),
                                        );
                                        md.insert(
                                            "budget".to_string(),
                                            AnalyticsValue::Int(total as i64),
                                        );
                                        let call_count = workflow_metrics.lock().await.call_count;
                                        md.insert(
                                            "agentCount".to_string(),
                                            AnalyticsValue::Int(call_count as i64),
                                        );
                                        bus_call
                                            .log_event(
                                                telemetry::tengu::workflow::BUDGET_CAP_EXCEEDED,
                                                md,
                                            )
                                            .await;
                                    }
                                    return wf_throw(&workflow_budget_exceeded_message(
                                        turn_spent, total,
                                    ));
                                }
                            }

                            let cap_failure = {
                                let mut metrics = workflow_metrics.lock().await;
                                if metrics.call_count >= WORKFLOW_AGENT_CAP {
                                    let should_emit = !metrics.cap_telemetry_emitted;
                                    metrics.cap_telemetry_emitted = true;
                                    Some((metrics.call_count, should_emit))
                                } else {
                                    metrics.call_count += 1;
                                    None
                                }
                            };
                            if let Some((call_count, should_emit)) = cap_failure {
                                if should_emit {
                                    let mut md: LogEventMetadata = HashMap::new();
                                    md.insert(
                                        "agentCount".to_string(),
                                        AnalyticsValue::Int(call_count as i64),
                                    );
                                    bus_call
                                        .log_event(telemetry::tengu::workflow::AGENT_CAP_EXCEEDED, md)
                                        .await;
                                }
                                return wf_throw(WORKFLOW_AGENT_CAP_MESSAGE);
                            }
                        }

                        let (
                            key,
                            prompt,
                            mut opts_json,
                            call_index,
                            label,
                            phase_index,
                            phase_title,
                        ) = match plan {
                            Plan::Resolve(spec) => {
                                return match resolve_nested_script(
                                    &spec,
                                    nested_fs.as_ref(),
                                    nested_plugin_workflows.as_deref(),
                                )
                                .await
                                {
                                    Ok(src) => workflow::strip_meta_export(&src),
                                    Err(_) => String::new(),
                                }
                            }
                            Plan::Cached {
                                result,
                                call_index,
                                label,
                                phase_index,
                                phase_title,
                            } => {
                                workflow_metrics.lock().await.record_cached(
                                    call_index,
                                    phase_index,
                                    phase_title.clone(),
                                    &result,
                                );
                                let tool_use_id = format!("workflow_agent_{call_index}_cached");
                                let cached_event = workflow::Progress::Agent {
                                    index: call_index,
                                    label: label.clone(),
                                    phase_index,
                                    phase_title,
                                    agent_id: None,
                                    model: None,
                                    state: workflow::AgentState::Cached,
                                    error: None,
                                    tool_use_id,
                                };
                                let mut update = workflow_progress_update(&cached_event);
                                update.tool_use_id =
                                    Some(format!("workflow_agent_{call_index}_cached"));
                                update.last_progress_at_ms = Some(unix_time_ms_now());
                                if let Some(ref tx) = ptx {
                                    emit_workflow_agent_snapshot(Some(tx), &update);
                                }
                                if let Some(ref tx) = live_tx {
                                    let _ = tx.send(update);
                                }
                                return result;
                            }
                            Plan::Live {
                                key,
                                prompt,
                                opts_json,
                                call_index,
                                label,
                                phase_index,
                                phase_title,
                            } => (
                                key,
                                prompt,
                                opts_json,
                                call_index,
                                label,
                                phase_index,
                                phase_title,
                            ),
                        };
                        let mut opts: Value = serde_json::from_str(&opts_json).unwrap_or(Value::Null);
                        if let Err(error) = structured_output_parse_retries(&opts) {
                            return wf_throw(error);
                        }
                        if opts.get("isolation").and_then(Value::as_str) == Some("remote") {
                            return wf_throw("agent({isolation:'remote'}) is not available in this build");
                        }
                        let agent_display_model = workflow_agent_display_model(&opts);
                        if let Some(at) = opts
                            .get("agentType")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                        {
                            let listing = spawner.agent_listing().await;
                            // The plugin loader registers agents as
                            // `<plugin>:<agent>` (manager.rs:1052-1055), but a
                            // plugin's own workflow scripts author BARE agentType
                            // names -- they must not have to know what namespace
                            // they were installed under. The namespace comes from
                            // `plugin_namespace_for_workflow`, which handles both
                            // the qualified id of a by-name launch and the bare
                            // `meta.name` the resume and desktop paths hand us,
                            // and is `None` for any workflow that is not
                            // plugin-owned.
                            //
                            // r1-workflow-runtime-15: the plugin's OWN agent wins.
                            // This used to check the bare name FIRST and only fall
                            // back to `<plugin>:<agent>` when it was absent, on the
                            // "closer scope wins" precedence `resolve_script_at`
                            // uses for workflow NAME resolution. That precedent
                            // does not transfer: there the user names the workflow,
                            // so their own file winning is their intent, whereas
                            // here the name is written INSIDE the plugin's script
                            // about the plugin's own component. Under bare-first, a
                            // user or project agent called `builder`, `designer`,
                            // `operator`, `tester` or `verifier` -- all names a
                            // person plausibly picks -- silently replaced the
                            // corresponding `lingxi-local-app:` agent for the whole
                            // local-app build, with no diagnostic anywhere. Bare
                            // names still resolve normally: the namespaced spelling
                            // is only preferred when the plugin actually registered
                            // one, so `general-purpose`/`Explore` and every
                            // non-plugin workflow are unaffected.
                            let namespaced = plugin_namespace_for_workflow(
                                &workflow_id,
                                nested_plugin_workflows.as_deref(),
                            )
                            .map(|namespace| format!("{namespace}:{at}"));
                            let qualified = namespaced
                                .as_deref()
                                .filter(|q| listing.iter().any(|e| e.agent_type == *q));
                            match qualified {
                                Some(qualified) => {
                                    if let Some(obj) = opts.as_object_mut() {
                                        obj.insert(
                                            "agentType".to_string(),
                                            Value::String(qualified.to_string()),
                                        );
                                    }
                                    opts_json = serde_json::to_string(&opts).unwrap_or(opts_json);
                                }
                                None if !listing.iter().any(|e| e.agent_type == at) => {
                                    let available = listing
                                        .iter()
                                        .map(|e| e.agent_type.clone())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    return wf_throw(&match namespaced {
                                        Some(namespaced) => format!(
                                            "agent({{agentType}}): agent type '{at}' not found (also tried '{namespaced}'). Available agents: {available}"
                                        ),
                                        None => format!(
                                            "agent({{agentType}}): agent type '{at}' not found. Available agents: {available}"
                                        ),
                                    });
                                }
                                None => {}
                            }
                        }
                        let inherit = SubagentInheritance {
                            tool_invoker,
                            budget,
                        };
                        let mut request = make_request(&subagent_type, &prompt, &opts_json);
                        request.origin_session_id = workflow_session_uuid
                            .as_deref()
                            .and_then(protocol::SessionId::parse_prefixed);
                        let queued_ms = unix_time_ms_now();
                        emit_workflow_agent_queued(
                            ptx.as_ref(),
                            live_tx.as_ref(),
                            call_index,
                            &label,
                            &prompt,
                            phase_index,
                            phase_title.clone(),
                            agent_display_model.clone(),
                            queued_ms,
                        );
                        let queued_at_ms = Some(queued_ms);
                        let observer_enabled =
                            live_tx.is_some() || (journal_writer.is_some() && key.is_some());
                        let observer = observer_enabled.then(|| {
                            Arc::new(WorkflowAgentLiveObserver::new_with_metrics(
                                ptx.clone(),
                                live_tx.clone(),
                                WorkflowProgressUpdate {
                                    kind: "workflow_agent".to_string(),
                                    index: call_index,
                                    title: None,
                                    message: None,
                                    label: Some(label.clone()),
                                    phase_index,
                                    phase_title: phase_title.clone(),
                                    agent_id: None,
                                    agent_type: Some(request.subagent_type.clone()),
                                    model: agent_display_model.clone(),
                                    fallback_model: None,
                                    state: Some("start".to_string()),
                                    error: None,
                                    tool_use_id: Some(format!("workflow_agent_{call_index}_queued")),
                                    queued_at_ms,
                                    started_at_ms: None,
                                    last_progress_at_ms: queued_at_ms,
                                    attempt: Some(1),
                                    last_attempt_reason: None,
                                    tokens: None,
                                    tool_calls: None,
                                    last_tool_name: None,
                                    last_tool_summary: None,
                                    prompt_preview: Some(prompt.chars().take(120).collect()),
                                },
                                journal_writer.clone().zip(key.clone()),
                                Some(workflow_metrics.clone()),
                                call_index,
                            ))
                        });
                        // Catalog/observer preparation above can await while
                        // another batch member fails accounting. Recheck at
                        // the final dispatch boundary, not only at batch entry.
                        if output_accounting_failed.load(Ordering::Acquire) {
                            return wf_throw("workflow output accounting failed; further dispatch is disabled");
                        }
                        let raw = if let Some(observer) = observer.clone() {
                            spawner
                                .spawn_workflow_with_observer(
                                    request,
                                    inherit,
                                    None,
                                    Some(
                                        observer
                                            as Arc<
                                                dyn platform_api::subagent_spawn::SubagentSpawnObserver,
                                            >,
                                    ),
                                    platform_api::subagent_spawn::WorkflowQueryWatchdog::default(),
                                )
                                .await
                        } else {
                            spawner.spawn(request, inherit).await
                        };
                        let terminal_error = subagent_failure_reason(&raw);
                        if let (Some(scope), Ok(result)) = (&output_scope, &raw) {
                            let recorded = known_workflow_agent_output(result).and_then(|tokens| {
                                tokens.map(|tokens| {
                                    scope.record_legacy(
                                        platform_api::WorkflowOutputEventId::WorkflowAgent {
                                            run_id: output_run_id.clone(),
                                            call_index,
                                        },
                                        tokens,
                                    )
                                }).transpose()
                            });
                            if let Err(error) = recorded {
                                output_accounting_failed.store(true, Ordering::Release);
                                return wf_throw(&error.to_string());
                            }
                        } else if let Ok(SubagentResult::Completed { usage, .. }) = &raw {
                            // Preserve the legacy pool's final-turn visible-output contract.
                            spent.fetch_add(usage.output_tokens, Ordering::Relaxed);
                        }
                        workflow_metrics.lock().await.record_result(
                            call_index,
                            phase_index,
                            phase_title.clone(),
                            &raw,
                        );
                        {
                            let (state, agent_id_str) = match &raw {
                                Ok(SubagentResult::Completed { agent_id, .. }) => {
                                    (workflow::AgentState::Done, Some(agent_id.to_string()))
                                }
                                Ok(
                                    SubagentResult::Failed { agent_id, .. }
                                    | SubagentResult::Killed { agent_id, .. },
                                ) => (workflow::AgentState::Error, Some(agent_id.to_string())),
                                Err(_) => (workflow::AgentState::Error, None),
                            };
                            let tool_use_id = if let Some(ref id) = agent_id_str {
                                format!("workflow_agent_{call_index}_{id}")
                            } else {
                                format!("workflow_agent_{call_index}_error")
                            };
                            let lifecycle_event = workflow::Progress::Agent {
                                index: call_index,
                                label: label.clone(),
                                phase_index,
                                phase_title,
                                agent_id: agent_id_str.clone(),
                                model: agent_display_model.clone(),
                                state: state.clone(),
                                error: terminal_error.clone(),
                                tool_use_id: tool_use_id.clone(),
                            };
                            let observer_state = if let Some(observer) = observer.as_ref() {
                                Some(observer.snapshot().await)
                            } else {
                                None
                            };
                            let observer_emitted_terminal = observer_state
                                .as_ref()
                                .and_then(|snapshot| snapshot.state.as_deref())
                                .is_some_and(|state| matches!(state, "done" | "error" | "cached"));
                            if !observer_enabled || raw.is_err() || !observer_emitted_terminal
                            {
                                let mut update = observer_state
                                    .unwrap_or_else(|| workflow_progress_update(&lifecycle_event));
                                update.agent_id = agent_id_str;
                                update.model = agent_display_model;
                                update.state = Some(state.as_str().to_string());
                                update.error = terminal_error.clone();
                                update.tool_use_id = Some(tool_use_id);
                                update.last_progress_at_ms = Some(unix_time_ms_now());
                                if let Some(ref tx) = ptx {
                                    emit_workflow_agent_snapshot(Some(tx), &update);
                                }
                                if let Some(ref tx) = live_tx {
                                    let _ = tx.send(update);
                                }
                            }
                        }
                        if opts.get("throwOnError").and_then(Value::as_bool) == Some(true) {
                            if let Some(error) = terminal_error {
                                return wf_throw(&format!("Workflow agent {label:?} failed: {error}"));
                            }
                        }
                        let journal_agent_id = match &raw {
                            Ok(SubagentResult::Completed { agent_id, .. }) => agent_id.to_string(),
                            _ => String::new(),
                        };
                        let result = result_to_string(raw);
                        if result != workflow::WF_NULL_SENTINEL {
                            if let (Some(j), Some(k)) = (journal.as_ref(), key) {
                                j.lock().unwrap().insert(k.clone(), result.clone());
                                if let Some(writer) = &journal_writer {
                                    writer.append_result(&k, &journal_agent_id, &result).await;
                                }
                            }
                        }
                        result
                    }
                }))
                .buffered(cap)
                .collect()
                .await;
                let _ = reply.send(results);
            }
        }
    }

    let outcome = outcome_rx.await.map_err(|_| {
        workflow::WorkflowError::Engine(
            "workflow script thread terminated without an outcome".into(),
        )
    })??;

    // Standalone callers (the public test/host seam) still emit here; the
    // production task passes the metrics out so it can preserve Claude Code's
    // completed-before-phase telemetry ordering.
    if emit_phase_telemetry_here {
        let metrics = workflow_metrics.lock().await.clone();
        emit_phase_completed(&bus, phase_telemetry_ctx.as_ref(), &metrics).await;
    }

    Ok(outcome)
}

/// Resolve a `workflow()` reference (`{ name }` or `{ scriptPath }`) to a script
/// source: `scriptPath` is read through the workflow filesystem; `name` resolves
/// under project `.lingxi/workflows` first, then the user config workflow
/// directory (`$LINGXI_CONFIG_DIR/workflows` or `~/.lingxi/workflows`), then
/// a wired plugin registry as the final tier.
async fn resolve_nested_script(
    spec: &Value,
    fs: Option<&Arc<dyn FileSystem>>,
    plugin_workflows: Option<&workflow::PluginWorkflowRegistry>,
) -> Result<String, String> {
    let fs = fs.ok_or_else(|| "no filesystem to resolve workflow()".to_string())?;
    if let Some(path) = spec
        .get("scriptPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return fs
            .read_file(path, None, None)
            .await
            .map(|fc| fc.content)
            .map_err(|e| e.to_string());
    }
    if let Some(name) = spec
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        for candidate in saved_workflow_candidates(name) {
            let candidate = candidate.to_string_lossy().into_owned();
            if PathBuf::from(&candidate).is_absolute() {
                match std::fs::read_to_string(&candidate) {
                    Ok(src) if !src.is_empty() => return Ok(src),
                    Ok(_) => continue,
                    Err(_) => {}
                }
            } else if let Ok(fc) = fs.read_file(&candidate, None, None).await {
                if !fc.content.is_empty() {
                    return Ok(fc.content);
                }
            }
        }
        if let Some(path) = plugin_workflows.and_then(|registry| registry.resolve(name)) {
            if let Ok(src) = std::fs::read_to_string(&path) {
                if !src.is_empty() {
                    return Ok(src);
                }
            }
        }
        return Err(format!("no saved workflow named '{name}'"));
    }
    Err("workflow() requires a name or scriptPath".to_string())
}

#[async_trait]
impl Task for LocalWorkflowHandler {
    async fn process_owner_ids(&self, task_id: &str) -> Vec<protocol::AgentId> {
        let owners = self
            .process_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(task_id)
            .cloned();
        owners
            .map(|owners| {
                owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }
    async fn has_live_worker(&self, task_id: &str) -> bool {
        self.workers.lock().await.contains_key(task_id)
    }

    fn name(&self) -> &str {
        HANDLER_NAME
    }

    fn task_type(&self) -> TaskType {
        TaskType::LocalWorkflow
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        // 1. Only the LocalWorkflow variant is accepted.
        let TaskSpawnInput::LocalWorkflow {
            session_uuid,
            workflow_id,
            script,
            resume_from_run_id,
            args: workflow_args,
            run_id: provided_run_id,
            invocation_mode,
            workflow_source,
            script_is_verbatim_builtin,
            transcript_subdir,
            launched_from_subagent,
            tool_use_id: _tool_use_id,
            creator_teammate_name: _,
            creator_team_name: _,
            creator_agent_id: _,
            scope,
        } = input
        else {
            return Err(TaskError::Internal(
                "local_workflow handler received a non-LocalWorkflow spawn input".into(),
            ));
        };

        // A workflow may carry a trusted originating session, but a malformed
        // value must never silently degrade to the process-wide budget view.
        // Validate before allocating task/spool state; `None` remains valid for
        // standalone hosts that intentionally have no session binding.
        if session_uuid
            .as_deref()
            .is_some_and(|raw| protocol::SessionId::parse_prefixed(raw).is_none())
        {
            return Err(TaskError::Internal(
                "workflow session_uuid must be a valid session id".into(),
            ));
        }

        // Capture output authority before allocating or awaiting task work.
        let output_scope = self
            .output_scopes
            .as_ref()
            .map(|scopes| {
                let session = session_uuid
                    .as_deref()
                    .and_then(protocol::SessionId::parse_prefixed)
                    .ok_or_else(|| {
                        TaskError::Internal("scoped workflow requires a canonical session".into())
                    })?;
                scopes
                    .capture(session)
                    .map_err(|error| TaskError::Internal(error.to_string()))
            })
            .transpose()?;

        // A local-app build may only run with a lease bound to the exact app
        // workspace. Do this validation before allocating task/spool state so
        // a malformed scope cannot start a prompt-heavy workflow with a
        // generic cwd or leave an orphaned spool file behind.
        //
        // `scope` is whatever the Host put on the spawn input -- a value only
        // a purpose constructor can produce, for an app id the Host resolved
        // itself. Deriving `app_id` from `workflow_args` here instead --
        // caller-supplied JSON, exactly like `workflow_id` -- is the exact
        // vector this migration exists to close (design §8.1: a custom
        // workflow must get nothing "即使伪造 meta.name 或 args.app_id"), so it
        // is deliberately NOT restored as a fallback: an unscoped run simply
        // takes no lease.
        let workspace_lease = if requires_workspace_lease(scope.as_ref()) {
            let app_id = scope
                .as_ref()
                .expect("requires_workspace_lease(Some(_)) implies scope is Some")
                .app_id()
                .to_string();
            let registry = self.workspace_leases.clone().ok_or_else(|| {
                TaskError::Internal(format!(
                    "{workflow_id} requires a workspace permission lease registry"
                ))
            })?;
            let data_root = self.workspace_root.clone().ok_or_else(|| {
                TaskError::Internal(format!("{workflow_id} requires an app data root"))
            })?;
            // AppService's persisted invariant is exactly
            // `apps/<id>/workspace`. Keep this derivation here, at the point
            // where the workflow's app_id is validated, so a workflow cannot
            // borrow the current session cwd or another app's workspace.
            let root = local_app_workspace_root(&data_root, &app_id);
            Some(
                registry
                    .begin_local_app(app_id, root)
                    .map_err(TaskError::Internal)?,
            )
        } else {
            None
        };

        // 2. Generate the task id (prefix 'w') and allocate its spool file.
        let task_id = crate::id::generate_task_id(TaskType::LocalWorkflow);
        let spool_path = self
            .output_manager
            .allocate(&task_id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        if spool_path.to_str().is_none() {
            return Err(TaskError::Internal("spool path is not valid UTF-8".into()));
        }

        // 3. Drive the workflow to completion inside a runtime-spawned worker
        //    (engine code must not call tokio::spawn directly — D17). The worker
        //    awaits `run_workflow_script`, spools the script's return value,
        //    reports the terminal status, and removes its own cancel record.
        let spawner = self.spawner.clone();
        let worktree_manager = self.worktree_manager.clone();
        let tool_invoker = self.tool_invoker.clone();
        // A background workflow keeps the budget ledger of the session that
        // launched it. Scope once at task spawn so every later agent()
        // dispatch inherits the same handle even after a clear/resume
        // switches the active router. Legacy/unparseable ids retain the
        // composition-root handle.
        let budget = session_uuid
            .as_deref()
            .and_then(protocol::SessionId::parse_prefixed)
            .and_then(|session_id| self.budget.scoped_for_session(session_id))
            .unwrap_or_else(|| self.budget.clone());
        let status_sink = self.status_sink.clone();
        let workflow_progress_sink = self.workflow_progress_sink.clone();
        let workers = self.workers.clone();
        let output_manager = self.output_manager.clone();
        let fs = ctx.fs.clone();
        let plugin_workflows = self.plugin_workflows.clone();
        let runtime = ctx.runtime.clone();
        let token_budget_total = self.token_budget_total;
        // The shared `budget.spent()` pool (main loop + all workflows), published
        // by the root once the orchestrator exists. `None` in tests ⇒ the run
        // uses its own private pool (own-spend only).
        let shared_pool = self
            .output_pool_cell
            .as_ref()
            .and_then(|c| c.get().cloned());
        // Snapshot the turn-start baseline (claude-code `R = xtr`) once, now, at
        // spawn — it is fixed for this workflow's life even as later turns update
        // the orchestrator's live baseline.
        let turn_start_baseline = self
            .turn_baseline_cell
            .as_ref()
            .and_then(|c| c.get())
            .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        let worker_spool_path = spool_path.clone();
        let worker_task_id = task_id.clone();
        let process_owners = Arc::new(StdMutex::new(HashSet::new()));
        self.process_owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(task_id.clone(), process_owners.clone());
        // Cooperative-cancel flag: shared between the script thread's engine
        // interrupt handler (via `run_workflow_script`) and the `WorkerCancel`
        // record `kill` flips. `false` until killed.
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (completion_tx, completion_rx) = oneshot::channel();
        // Cloned for kill-detection in tengu_workflow_completed status derivation
        // (oracle §7: `abortController?.signal.aborted ? "killed" : error ? "failed" : "completed"`).
        let completed_cancel = cancel.clone();
        let worker_bus = self.bus.clone();
        // Parse meta fields from the script NOW (before moving `script` into the
        // worker closure) so they are available for both `launched` and `completed`
        // telemetry without re-parsing inside the async closure.
        let meta_name: Option<String> = workflow::meta_string_value(&script, "name");
        let meta_description: Option<String> = workflow::meta_string_value(&script, "description");
        // phase_count: number of declared phases in `meta.phases`. This mirrors
        // the oracle's `c.meta.phases?.length ?? 0`: it is the static declaration,
        // not the number of runtime `phase()` calls.
        let meta_phase_count = workflow::meta_array_len(&script, "phases").unwrap_or(0) as i64;
        let script_size_chars = workflow_script_size_chars(&script);
        let workspace_lease_token = workspace_lease
            .as_ref()
            .map(permission::WorkspacePermissionLease::token);
        let worker = Box::pin({
            async move {
                let _completion_signal = WorkerCompletionSignal::new(completion_tx);
                let _workspace_lease = workspace_lease;
                let registered = wait_for_workflow_registration(
                    &status_sink,
                    &runtime,
                    &worker_task_id,
                    &worker_cancel,
                )
                .await;
                if !registered {
                    workers.lock().await.remove(&worker_task_id);
                    return;
                }

                status_sink
                    .set_status(&worker_task_id, TaskStatus::Running)
                    .await;

                // Resume journal: a stable `wf_…` run id keys a per-(prompt, opts)
                // `agent()` result cache. A new run mints one; a resume reuses the
                // caller's id and pre-loads its journal so unchanged agents replay
                // instead of re-spawning. The id is surfaced to the task output so a
                // later call can pass it back as `resumeFromRunId`.
                // A resume reuses the caller's id; a fresh run uses the
                // launcher-minted id (so the Workflow tool result can return it)
                // and only mints one here as a last resort (direct test spawns).
                // Fresh-mint shape matches claude-code 2.1.195
                // `wf_${randomUUID().slice(0,12)}` = `wf_` + 8 hex + `-` + 3 hex.
                let run_id = resume_from_run_id
                    .clone()
                    .or(provided_run_id)
                    .unwrap_or_else(|| {
                        let r = rand::random::<u64>();
                        format!("wf_{:08x}-{:03x}", (r >> 32) as u32, (r as u32) & 0xfff)
                    });

                // tengu_workflow_launched — oracle §7 exact payload.
                // NOTE: `workflow_run_id` is NOT present on `launched` (oracle §7 shows
                // it only on `completed` and `phase_completed`). `invocation_mode`,
                // `workflow_source`, `workflow_name`, `workflow_description`,
                // `phase_count`, `launched_from_subagent`, `has_args`, `is_resume`,
                // and `script_size_chars` are all present.
                // `launched_from_subagent`: oracle `t.agentId != null`. LingXi does not
                // thread the calling agent id to the task handler — this field comes
                // from the spawn input where it was set by the launcher.
                {
                    let mut md: LogEventMetadata = HashMap::new();
                    md.insert(
                        "invocation_mode".to_string(),
                        AnalyticsValue::String(
                            invocation_mode
                                .clone()
                                .unwrap_or_else(|| "inline".to_string()),
                        ),
                    );
                    md.insert(
                        "workflow_source".to_string(),
                        AnalyticsValue::String(
                            workflow_source
                                .clone()
                                .unwrap_or_else(|| "inline".to_string()),
                        ),
                    );
                    md.insert(
                        "workflow_name".to_string(),
                        AnalyticsValue::String(telemetry_workflow_name(
                            workflow_source.as_deref(),
                            script_is_verbatim_builtin,
                            meta_name.as_deref(),
                        )),
                    );
                    md.insert(
                        "workflow_description".to_string(),
                        AnalyticsValue::String(telemetry_workflow_description(
                            workflow_source.as_deref(),
                            script_is_verbatim_builtin,
                            meta_description.as_deref(),
                        )),
                    );
                    md.insert(
                        "phase_count".to_string(),
                        AnalyticsValue::Int(meta_phase_count),
                    );
                    md.insert(
                        "launched_from_subagent".to_string(),
                        AnalyticsValue::Bool(launched_from_subagent),
                    );
                    md.insert(
                        "has_args".to_string(),
                        AnalyticsValue::Bool(workflow_args.is_some()),
                    );
                    md.insert(
                        "is_resume".to_string(),
                        AnalyticsValue::Bool(resume_from_run_id.is_some()),
                    );
                    md.insert(
                        "script_size_chars".to_string(),
                        AnalyticsValue::Int(script_size_chars),
                    );
                    worker_bus
                        .log_event(telemetry::tengu::workflow::LAUNCHED, md)
                        .await;
                }

                let journal_path = worker_spool_path
                    .parent()
                    .map(|d| d.join(format!("workflow-{run_id}.json")));
                let journal_writer =
                    transcript_subdir
                        .as_ref()
                        .map(|directory| WorkflowJournalWriter {
                            path: directory.join("journal.jsonl"),
                            fs: fs.clone(),
                        });
                if let Some(writer) = &journal_writer {
                    writer.ensure_exists().await;
                }
                let mut cache: HashMap<String, String> = HashMap::new();
                if resume_from_run_id.is_some() {
                    if let Some(writer) = &journal_writer {
                        cache = writer.load_results().await;
                    }
                    // Compatibility with runs created before the append-only
                    // transcript journal existed.
                    if cache.is_empty() {
                        if let Some(p) = journal_path.as_ref().and_then(|p| p.to_str()) {
                            if let Ok(fc) = fs.read_file(p, None, None).await {
                                if let Ok(loaded) =
                                    serde_json::from_str::<HashMap<String, String>>(&fc.content)
                                {
                                    cache = loaded;
                                }
                            }
                        }
                    }
                    // tengu_workflow_journal_started_hit_respawn — resume loaded a
                    // non-empty journal: previously-started agents will be replayed.
                    if !cache.is_empty() {
                        let mut md: LogEventMetadata = HashMap::new();
                        md.insert(
                            "attempts".to_string(),
                            AnalyticsValue::Int(cache.len() as i64),
                        );
                        worker_bus
                            .log_event(telemetry::tengu::workflow::JOURNAL_STARTED_HIT_RESPAWN, md)
                            .await;
                    }
                }
                let journal = Arc::new(std::sync::Mutex::new(cache));
                let _ = output_manager
                    .append(&worker_spool_path, &format!("runId: {run_id}\n"))
                    .await;

                // Live progress: `phase()`/`log()` lines are spooled to the task
                // output as they happen (drained concurrently with the run), so a
                // long workflow's progress is visible via TaskOutput before it
                // finishes. The drainer ends when `run_workflow_script` drops its
                // sender on return.
                let (ptx, mut prx) = mpsc::unbounded_channel::<String>();
                let (wptx, mut wprx) = mpsc::unbounded_channel::<WorkflowProgressUpdate>();
                let workflow_metrics =
                    Arc::new(tokio::sync::Mutex::new(WorkflowRunMetrics::default()));
                let prog_output = output_manager.clone();
                let prog_spool = worker_spool_path.clone();
                let worker_task_id_for_progress = worker_task_id.clone();
                let run_id_for_progress = run_id.clone();
                let drain = async move {
                    while let Some(line) = prx.recv().await {
                        let _ = prog_output.append(&prog_spool, &format!("{line}\n")).await;
                    }
                };
                let live_drain = async move {
                    while let Some(progress) = wprx.recv().await {
                        if let Some(ref sink) = workflow_progress_sink {
                            sink.emit_workflow_progress(
                                &worker_task_id_for_progress,
                                &run_id_for_progress,
                                progress,
                            )
                            .await;
                        }
                    }
                };
                let run_start = std::time::Instant::now();
                let workflow_tool_invoker: Arc<dyn ToolInvoker> =
                    if let Some(token) = workspace_lease_token {
                        Arc::new(WorkspaceLeaseToolInvoker {
                            inner: tool_invoker.clone(),
                            token,
                        })
                    } else {
                        tool_invoker.clone()
                    };
                let workflow_spawner: Arc<dyn SubagentSpawner> =
                    if let Some(worktree) = worktree_manager.clone() {
                        Arc::new(WorkflowIsolationSpawner {
                            process_owners: process_owners.clone(),
                            inner: spawner.clone(),
                            worktree: Some(worktree),
                            slug_prefix: worker_task_id.clone(),
                            sequence: AtomicU64::new(0),
                            transcript_subdir: transcript_subdir.clone(),
                        })
                    } else {
                        Arc::new(WorkflowIsolationSpawner {
                            process_owners: process_owners.clone(),
                            inner: spawner.clone(),
                            worktree: None,
                            slug_prefix: worker_task_id.clone(),
                            sequence: AtomicU64::new(0),
                            transcript_subdir: transcript_subdir.clone(),
                        })
                    };
                let phase_telemetry_ctx = PhaseTelemetryCtx {
                    run_id: run_id.clone(),
                    workflow_source: workflow_source.clone(),
                    script_is_verbatim_builtin,
                    workflow_name: meta_name.clone(),
                    invocation_mode: invocation_mode.clone(),
                };
                let run = run_workflow_script_with_live_updates_recorded(
                    &script,
                    DEFAULT_WORKFLOW_SUBAGENT,
                    &workflow_id,
                    workflow_spawner,
                    workflow_tool_invoker,
                    budget,
                    Some(ptx),
                    Some(wptx),
                    Some(journal.clone()),
                    journal_writer,
                    token_budget_total,
                    shared_pool,
                    turn_start_baseline,
                    NestedConfig {
                        allow_nested: true,
                        args: workflow_args,
                        fs: Some(fs.clone()),
                        plugin_workflows: plugin_workflows.clone(),
                        session_uuid: session_uuid.clone(),
                    },
                    worker_cancel,
                    worker_bus.clone(),
                    None,
                    // Pass the phase telemetry context so run_workflow_script can
                    // emit tengu_workflow_phase_completed with the correct run_id,
                    // workflow_source, workflow_name, and built-in-source gate.
                    Some(phase_telemetry_ctx.clone()),
                    Some(workflow_metrics.clone()),
                    output_scope,
                );
                let (outcome, (), ()) = tokio::join!(run, drain, live_drain);
                // Natural completion and TaskStop race on the worker map. Once
                // finalization wins, cancellation must not tear down the
                // telemetry/journal/outcome commit window. If kill already
                // removed the record, it owns the terminal Killed transition.
                let may_finalize = {
                    let mut workers = workers.lock().await;
                    match workers.get_mut(&worker_task_id) {
                        Some(rec) => {
                            rec.finalizing = true;
                            true
                        }
                        None => false,
                    }
                };
                if !may_finalize {
                    return;
                }
                let elapsed_ms = run_start.elapsed().as_millis() as i64;
                let workflow_metrics_snapshot = workflow_metrics.lock().await.clone();

                // tengu_workflow_completed — oracle §7 exact payload.
                // `status` derivation: `abortController?.signal.aborted ? "killed"
                //   : k.error ? "failed" : "completed"` (oracle §7).
                // `agent_count`: all real `agent()` calls, including cache hits;
                // total token/tool-call rollups come from terminal subagent results.
                {
                    let agent_count_val = workflow_metrics_snapshot.call_count as i64;
                    let status_str = if completed_cancel.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        "killed"
                    } else {
                        match &outcome {
                            Ok(_) => "completed",
                            Err(_) => "failed",
                        }
                    };
                    let mut md: LogEventMetadata = HashMap::new();
                    md.insert(
                        "workflow_run_id".to_string(),
                        AnalyticsValue::String(run_id.clone()),
                    );
                    md.insert(
                        "workflow_source".to_string(),
                        AnalyticsValue::String(
                            workflow_source
                                .clone()
                                .unwrap_or_else(|| "inline".to_string()),
                        ),
                    );
                    md.insert(
                        "workflow_name".to_string(),
                        AnalyticsValue::String(telemetry_workflow_name(
                            workflow_source.as_deref(),
                            script_is_verbatim_builtin,
                            meta_name.as_deref(),
                        )),
                    );
                    md.insert(
                        "workflow_description".to_string(),
                        AnalyticsValue::String(telemetry_workflow_description(
                            workflow_source.as_deref(),
                            script_is_verbatim_builtin,
                            meta_description.as_deref(),
                        )),
                    );
                    md.insert(
                        "status".to_string(),
                        AnalyticsValue::String(status_str.to_string()),
                    );
                    md.insert(
                        "agent_count".to_string(),
                        AnalyticsValue::Int(agent_count_val),
                    );
                    md.insert(
                        "total_tokens".to_string(),
                        AnalyticsValue::Int(workflow_metrics_snapshot.total_tokens as i64),
                    );
                    md.insert(
                        "total_tool_calls".to_string(),
                        AnalyticsValue::Int(workflow_metrics_snapshot.total_tool_calls as i64),
                    );
                    md.insert("duration_ms".to_string(), AnalyticsValue::Int(elapsed_ms));
                    worker_bus
                        .log_event(telemetry::tengu::workflow::COMPLETED, md)
                        .await;
                    emit_phase_completed(
                        &worker_bus,
                        Some(&phase_telemetry_ctx),
                        &workflow_metrics_snapshot,
                    )
                    .await;
                }
                // Note: phase telemetry is emitted after completed so event order
                // matches Claude Code's completed-before-phase sequence.

                // Persist the journal (new + replayed results) under the run id so a
                // later resume can replay them. Serialise before any await so the std
                // Mutex guard never crosses an await point.
                let serialized = serde_json::to_string(&*journal.lock().unwrap()).ok();
                if let (Some(p), Some(s)) = (
                    journal_path.as_ref().and_then(|p| p.to_str()),
                    serialized.as_ref(),
                ) {
                    let _ = fs.write_file(p, s).await;
                }

                // Publish the structured terminal payload before status so the
                // registry's notification drain cannot observe a result-less
                // completion. Failures remain separate from the script result,
                // matching Claude's workflow notification shape.
                let (agents_done, agents_error, agents_skipped, agents_empty_result) =
                    workflow_metrics_snapshot.terminal_counts();
                let terminal_outcome = match &outcome {
                    Ok(out) => platform_api::task_registry::WorkflowTerminalOutcome {
                        result: out.result.clone(),
                        failures: out.failures.clone(),
                        agent_count: workflow_metrics_snapshot.call_count,
                        total_tokens: workflow_metrics_snapshot.total_tokens,
                        total_tool_calls: workflow_metrics_snapshot.total_tool_calls,
                        duration_ms: elapsed_ms as u64,
                        agents_done,
                        agents_error,
                        agents_skipped,
                        agents_empty_result,
                        progress_counts_available: true,
                        ..Default::default()
                    },
                    Err(error) => platform_api::task_registry::WorkflowTerminalOutcome {
                        error: Some(error.to_string()),
                        agent_count: workflow_metrics_snapshot.call_count,
                        total_tokens: workflow_metrics_snapshot.total_tokens,
                        total_tool_calls: workflow_metrics_snapshot.total_tool_calls,
                        duration_ms: elapsed_ms as u64,
                        agents_done,
                        agents_error,
                        agents_skipped,
                        agents_empty_result,
                        progress_counts_available: true,
                        ..Default::default()
                    },
                };
                // The Workflow tool result is the script's return value; spool
                // only that value. Spool I/O is best-effort — a write failure
                // must not mask the result.
                let (payload, status) = match outcome {
                    Ok(out) => (out.result.unwrap_or_default(), TaskStatus::Completed),
                    Err(e) if completed_cancel.load(std::sync::atomic::Ordering::Relaxed) => {
                        (e.to_string(), TaskStatus::Killed)
                    }
                    Err(e) => (e.to_string(), TaskStatus::Failed),
                };
                if !payload.is_empty() {
                    let _ = output_manager.append(&worker_spool_path, &payload).await;
                }

                status_sink
                    .finish_workflow_terminal(&worker_task_id, terminal_outcome, status)
                    .await;
                workers.lock().await.remove(&worker_task_id);
            }
        });

        let mut workers = self.workers.lock().await;
        let bg_handle = ctx
            .runtime
            .spawn(&format!("{HANDLER_NAME}:{task_id}"), worker)
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;

        workers.insert(
            task_id.clone(),
            WorkerCancel {
                handle: bg_handle,
                runtime: ctx.runtime.clone(),
                cancel,
                completion_rx: StdMutex::new(Some(completion_rx)),
                finalizing: false,
            },
        );
        drop(workers);

        // 4. Synchronous cleanup seam (claude-code `registerCleanup` parity): the
        //    closure cannot await, so it records the task id for later async
        //    drain. This must not silently lose cancellation on lock contention.
        let cleanup_pending = self.pending_kill.clone();
        let cleanup_task_id = task_id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            cleanup_pending
                .lock()
                .unwrap()
                .push(cleanup_task_id.clone());
        });

        Ok(TaskHandle::new(task_id, Some(cleanup)))
    }

    async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
        // Finalization and kill arbitrate on the same worker-map lock. A worker
        // that already owns finalization must finish its atomic outcome/status
        // commit; otherwise kill removes the record and owns Killed.
        let rec = {
            let mut workers = self.workers.lock().await;
            if workers.get(task_id).is_some_and(|rec| rec.finalizing) {
                return Ok(());
            }
            workers.remove(task_id)
        };
        if let Some(rec) = rec {
            cancel_workflow_worker(rec).await?;
        }
        if !self.status_sink.is_terminal(task_id).await {
            self.status_sink
                .set_status(task_id, TaskStatus::Killed)
                .await;
        }
        Ok(())
    }

    async fn drain_shutdown(&self) -> Result<(), TaskError> {
        let completions = {
            let workers = self.workers.lock().await;
            workers
                .values()
                .filter_map(|worker| {
                    worker
                        .completion_rx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                })
                .collect::<Vec<_>>()
        };
        for completion in completions {
            let _ = completion.await;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "local_workflow_test.rs"]
mod local_workflow_test;

#[cfg(test)]
#[path = "local_workflow_accounting_test.rs"]
mod local_workflow_accounting_test;
