//! `Task*` tools — two distinct products sharing one tool-name surface.
//!
//! claude-code source:
//! `claude-code/src/tools/Task{Create,Get,List,Update}Tool/*` (the Product-A
//! todo-list tools, backed by `utils/tasks.ts`) plus `TaskStopTool.ts` /
//! `TaskOutputTool.tsx` (the Product-B background-task tools).
//!
//! ## Two products, one name surface (divergence note — sub-batch [6])
//!
//! - **Product-A V2 (todo store):** `TaskCreate` / `TaskGet` / `TaskList` /
//!   `TaskUpdate` are the claude-code todo-list tools. They persist through the
//!   file-backed [`crate::todo_store::TodoStore`] (decimal ids `"1".."N"`,
//!   `lingxi_core::TodoState` = `pending`/`in_progress`/`completed`). They do NOT use
//!   `validate_task_id`, `TASK_TYPES`, or `TASK_STATUSES`.
//! - **Product-B (background-registry):** `TaskStop` / `TaskOutput` dispatch the
//!   M1 background `TaskRegistry` (9-char `[bartwmdksfe][0-9a-z]{8}` ids).
//!
//! `validate_task_id` / `TASK_TYPES` / `TASK_STATUSES` are Product-B SHAPES,
//! not Product-B code paths: neither `TaskStop` nor `TaskOutput` calls them.
//! Both resolve an id by asking the registry and report `not found` when it
//! does not answer — which is what the oracle does too (`RZn` reads through
//! `EK` rather than shape-checking first). The symbols exist for the locked
//! `parity_agent_task_tools` fixture, which pins the id grammar; saying they
//! "back these two tools" claimed a call site that has never existed.
//!
//! All six tool-name constants stay reachable at `tool_task::task::*`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use lingxi_core::host::task_registry::TaskRegistryError;
use lingxi_core::TodoState;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Map, Value};
use telemetry::pii::Verified;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    TASK_CREATE_COMPLETED, TASK_CREATE_FAILED, TASK_CREATE_STARTED, TASK_GET_COMPLETED,
    TASK_GET_FAILED, TASK_GET_STARTED, TASK_LIST_COMPLETED, TASK_LIST_STARTED,
    TASK_OUTPUT_COMPLETED, TASK_OUTPUT_FAILED, TASK_OUTPUT_STARTED, TASK_STOP_COMPLETED,
    TASK_STOP_FAILED, TASK_STOP_STARTED, TASK_UPDATE_COMPLETED, TASK_UPDATE_FAILED,
    TASK_UPDATE_STARTED,
};
use telemetry::AnalyticsBus;

use crate::todo_store::{TodoStore, TodoTask};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    CoercedInput, DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult,
    ToolError, ToolStaticContext, ValidationError,
};
use tool_api::BuiltinToolContext;

/// Tool name `'TaskCreate'` (claude-code `TASK_CREATE_TOOL_NAME`).
pub const TASK_CREATE_TOOL_NAME: &str = "TaskCreate";
/// Tool name `'TaskGet'` (claude-code `TASK_GET_TOOL_NAME`).
pub const TASK_GET_TOOL_NAME: &str = "TaskGet";
/// Tool name `'TaskList'` (claude-code `TASK_LIST_TOOL_NAME`).
pub const TASK_LIST_TOOL_NAME: &str = "TaskList";
/// Tool name `'TaskUpdate'` (claude-code `TASK_UPDATE_TOOL_NAME`).
pub const TASK_UPDATE_TOOL_NAME: &str = "TaskUpdate";
/// Tool name `'TaskStop'` (claude-code inline declaration).
pub const TASK_STOP_TOOL_NAME: &str = "TaskStop";
/// Tool name `'TaskOutput'` (claude-code `TASK_OUTPUT_TOOL_NAME`).
pub const TASK_OUTPUT_TOOL_NAME: &str = "TaskOutput";

/// The 10 background task-type wire strings — the nine claude-code variants
/// plus LingXi's `local_fusion`. Byte-aligned with `tasks::TaskType` variants
/// (snake_case). The array has held ten entries since `local_fusion` landed;
/// the count in this sentence had not been updated.
///
// Product-B SHAPE — no production caller; see the module header.
pub const TASK_TYPES: &[&str] = &[
    "local_bash",
    "local_agent",
    "remote_agent",
    "in_process_teammate",
    "local_workflow",
    "monitor_mcp",
    "monitor_ws",
    "mcp_task",
    "dream",
    "auto_mode_scan",
    "local_fusion",
];

/// The 5 background task-status wire strings.
///
// Product-B SHAPE — no production caller; see the module header.
pub const TASK_STATUSES: &[&str] = &["pending", "running", "completed", "failed", "killed"];

/// Validate the task-id format `[bartwmdksfe][0-9a-z]{8}` (9 chars total) —
/// nine claude-code type prefixes plus LingXi's `f` (`local_fusion`).
///
/// NO production caller: this pins the id grammar for the locked
/// `parity_agent_task_tools` fixture. `TaskStop`/`TaskOutput` resolve ids
/// through the registry instead. V2 (Product-A) task ids are decimal strings
/// and MUST NOT route through this.
///
/// # Errors
/// Returns a locked human-readable error string on mismatch.
pub fn validate_task_id(s: &str) -> Result<(), String> {
    if s.chars().count() != 9 {
        return Err(format!(
            "Task: malformed task_id '{s}' (expected 9-char [bartwmdksfe][0-9a-z]{{8}})"
        ));
    }
    let mut chars = s.chars();
    let prefix = chars.next().expect("len==9");
    if !"bartwmdksfe".contains(prefix) {
        return Err(format!(
            "Task: malformed task_id '{s}' (expected 9-char [bartwmdksfe][0-9a-z]{{8}})"
        ));
    }
    for c in chars {
        if !(c.is_ascii_digit() || (c.is_ascii_lowercase() && c.is_ascii_alphabetic())) {
            return Err(format!(
                "Task: malformed task_id '{s}' (expected 9-char [bartwmdksfe][0-9a-z]{{8}})"
            ));
        }
    }
    Ok(())
}

/// Generate a fresh task-id matching the task-registry format
/// (`[bartwmdksfe][0-9a-z]{8}`).
///
/// Mirrors `tasks::id::generate_task_id` without taking the
/// cyclic dep on `lingxi-tasks`. Retained for test fixtures + parity
/// (the regex test in this file generates ids locally to verify they
/// match the locked regex without depending on the registry being wired).
#[cfg(test)]
fn fresh_task_id(prefix: char) -> String {
    use rand::Rng;
    const ALPHA: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    let suffix: String = (0..8)
        .map(|_| ALPHA[rng.random_range(0..ALPHA.len())] as char)
        .collect();
    format!("{prefix}{suffix}")
}

fn fresh_invocation_id() -> String {
    tool_api::util::ids::ulid_or_uuid()
}

/// Civil date `(year, month, day)` from days since the Unix epoch — port of
/// Howard Hinnant's `civil_from_days`. Valid for the proleptic Gregorian
/// calendar; we only ever feed it non-negative (post-1970) day counts.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// ISO-8601 UTC timestamp with millisecond precision
/// (e.g. `2026-06-07T12:34:56.789Z`) — port of TS `new Date().toISOString()`.
/// Std-only; deterministic for a fixed `SystemTime`. Mirrors the proven
/// `tools/ui/src/brief.rs` impl (kept local to avoid a cross-crate dep).
fn iso8601_utc(t: std::time::SystemTime) -> String {
    let dur = t
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

// ==== Product-A V2 gating (sub-batch [2]) ===================================

/// Port of `isEnvTruthy(process.env[key])` (`envUtils.ts:32-37`): lower-cased,
/// trimmed value ∈ {`1`,`true`,`yes`,`on`}.
fn env_truthy(key: &str) -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var(key).ok().as_deref())
}

/// Pure core of [`is_todo_v2_enabled`] — 1:1 with the binary's tasks-v2 gate,
/// spelled `TE()` in v2.1.183 and `X_()` in 2.1.263
/// (`src_160357157.js` @10464):
/// `function X_(){if(a.CLAUDE_CODE_ENABLE_TASKS===!1)return!1;return!0}`,
/// previously `function TE(){if(_l(process.env.LINGXI_ENABLE_TASKS))return!1;return!0}`.
/// i.e. V2-Task-tools-enabled = NOT (the env normalizes to `0`/`false`/`no`/`off`).
/// `_l` = [`lingxi_core::host::env::is_env_defined_falsy`] (byte-exact: `e===void 0`⇒false,
/// boolean⇒`!e`, else lowercased+trimmed ∈ {`0`,`false`,`no`,`off`}).
///
/// The 2.1.263 rename is NOT a behaviour change: `CLAUDE_CODE_ENABLE_TASKS` is
/// declared `I.triBool()`, whose transform is `Ie(e)?!0:po(e)?!1:void 0`, and
/// `po` is the old `_l` verbatim
/// (`["0","false","no","off"].includes(String(t).toLowerCase().trim())`,
/// `src_156606933.js` @922).
///
/// There is NO non-interactive term in the binary — the prior
/// `enable_tasks_env || !non_interactive` formula was stale (wrong/opposite
/// default for unset+non-interactive and for interactive+`false`/`no`/`off`).
fn todo_v2_enabled_inner(enable_tasks_env_defined_falsy: bool) -> bool {
    !enable_tasks_env_defined_falsy
}

/// claude-code `sut(e,t)` (`src_160988549.js` @3386470):
///
/// ```js
/// function sut(e,t){if(e===void 0)return!0;return e===t}
/// ```
///
/// `e` is the CALLER's agent id and `t` the task's owner. An absent caller is
/// the main session, which may stop anything; otherwise the two must match.
/// Note what this does NOT do: it never walks a parent chain, so an agent
/// cannot stop its own child's background task — only the child itself or the
/// main session can.
fn caller_may_stop(caller: Option<&str>, owner: Option<&str>) -> bool {
    match caller {
        None => true,
        Some(caller) => Some(caller) == owner,
    }
}

/// claude-code `iue(e){return e??"main session"}` (@3386524) — how an ownerless
/// task is named in the refusal.
const NO_OWNER_DISPLAY: &str = "main session";

/// The rosters a "no task found" message names, gathered once.
///
/// Ports the data half of claude-code `Mut` (@3595720) and `JFe` (@3594626).
/// `named_agents` is the agent-name registry's KEYS (the names, which is what
/// `wzo` reports), while its VALUES are passed to the registry so the
/// background-agent roster can exclude them — a named agent is reported once,
/// under its name.
async fn not_found_rosters(
    ctx: &tool_api::BuiltinToolContext,
    registry: &std::sync::Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    caller_agent_id: Option<&str>,
) -> (
    Vec<String>,
    lingxi_core::host::task_registry::TaskNotFoundRosters,
) {
    // `wzo`: a registry name counts only while its agent is actually running.
    let mut named_agents = Vec::new();
    let mut named_ids = Vec::new();
    if let Some(names) = ctx.agent_name_registry.as_ref() {
        for (name, agent_id) in names.list().await {
            let id = agent_id.to_string();
            match registry.get(&id).await {
                Ok(Some(record)) => {
                    if record.task_type == "local_agent"
                        && (record.status == "running" || record.is_parked)
                    {
                        named_agents.push(name);
                    }
                    named_ids.push(record.task_id);
                }
                _ => named_ids.push(id),
            }
        }
    }
    named_agents.sort();
    let rosters = registry
        .not_found_rosters(caller_agent_id, &named_ids)
        .await;
    (named_agents, rosters)
}

/// claude-code `Mut` (@3595720) — TaskStop's "no task found" message.
///
/// Four independent appends in a fixed order; each clause is its own `if`, not
/// an else-if, so a message can carry all of them. The `. Did you mean: X?`
/// clause comes from the registry's `XFe`/`Szo` name resolver.
async fn task_stop_not_found_message(
    ctx: &tool_api::BuiltinToolContext,
    registry: &std::sync::Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    requested: &str,
    suggestion: Option<&str>,
    caller_agent_id: Option<&str>,
) -> String {
    use lingxi_core::host::display::sanitize_display;
    let (named_agents, rosters) = not_found_rosters(ctx, registry, caller_agent_id).await;
    let mut message = format!("No task found with ID: {}", sanitize_display(requested));
    if let Some(suggestion) = suggestion {
        message.push_str(&format!(
            ". Did you mean: {}?",
            sanitize_display(suggestion)
        ));
    }
    if !rosters.running_teammates.is_empty() {
        message.push_str(&format!(
            ". Running teammates: {}",
            join_sanitized(&rosters.running_teammates)
        ));
    }
    if !named_agents.is_empty() {
        message.push_str(&format!(
            ". Running named agents: {}",
            join_sanitized(&named_agents)
        ));
    }
    message.push_str(&background_agents_clause(&rosters));
    message
}

/// claude-code `SWn` (@3695814) — TaskOutput's "no task found" message.
///
/// Deliberately NOT the same builder as TaskStop's. `SWn` interpolates the
/// requested id RAW (`${e}`, not `w1(e)`) and appends ONLY the background-agent
/// clause. Folding the two together would diverge on both axes at once.
async fn task_output_not_found_message(
    ctx: &tool_api::BuiltinToolContext,
    registry: &std::sync::Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    requested: &str,
    caller_agent_id: Option<&str>,
) -> String {
    let (_named, rosters) = not_found_rosters(ctx, registry, caller_agent_id).await;
    format!(
        "No task found with ID: {requested}{}",
        background_agents_clause(&rosters)
    )
}

/// `JFe`'s tail: the clause, or an empty string when the roster is empty.
fn background_agents_clause(
    rosters: &lingxi_core::host::task_registry::TaskNotFoundRosters,
) -> String {
    if rosters.background_agents.is_empty() {
        return String::new();
    }
    // Entries are pre-rendered `{id} ({description})` by the registry, whose
    // description already went through the sanitizer; joining them raw here
    // matches `d.join(", ")`.
    format!(
        ". Running background agents: {}",
        rosters.background_agents.join(", ")
    )
}

fn join_sanitized(values: &[String]) -> String {
    values
        .iter()
        .map(|value| lingxi_core::host::display::sanitize_display(value))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether the Product-A V2 Task tools are advertised (and `TodoWrite` hidden).
///
/// Port of `isTodoV2Enabled()` (binary `TE()`, 2.1.263 `X_()`): enabled UNLESS
/// `LINGXI_ENABLE_TASKS` is a *defined falsy* value (`0`/`false`/`no`/`off`,
/// case-insensitive, trimmed). Unset, empty, or any other value ⇒ enabled.
///
/// This is only HALF the advertise decision: the four tasks-dir Task tools are
/// gated on `h3(){return X_()&&OO()}` and `TodoWrite` on `!X_()&&OO()`, so the
/// `OO()` model gate is applied as a separate conjunct at each call site — see
/// [`task_tools_enabled`] / [`todo_write_enabled`].
///
/// The `ctx` parameter is retained for the `Tool::is_enabled` signature but is
/// unused — the binary's gate reads only the environment, no
/// session/interactive signal.
#[must_use]
pub fn is_todo_v2_enabled(_ctx: &ToolStaticContext) -> bool {
    todo_v2_enabled_inner(lingxi_core::host::env::is_env_defined_falsy(
        std::env::var("LINGXI_ENABLE_TASKS").ok().as_deref(),
    ))
}

/// claude-code `h3(){return X_()&&OO()}` — whether the four tasks-dir Task
/// tools are advertised.
///
/// `OO()` (`tool_api::todo_tools_gate`) is a SEPARATE conjunct, not something
/// folded into [`is_todo_v2_enabled`]: the oracle spells TodoWrite
/// `isEnabled(){return!X_()&&OO()}`, so `OO()` multiplies BOTH polarities of the
/// V1/V2 mutex. Folding it into `X_()` would hide the four Task tools and
/// resurrect TodoWrite in the same move — the exact opposite of the CHANGELOG's
/// "no longer available … TodoWrite".
#[must_use]
pub fn task_tools_enabled(ctx: &ToolStaticContext) -> bool {
    is_todo_v2_enabled(ctx) && tool_api::todo_tools_enabled(ctx)
}

/// claude-code TodoWrite `isEnabled(){return!X_()&&OO()}` — the V1 side of the
/// mutex, gated by the same `OO()` term as [`task_tools_enabled`].
#[must_use]
pub fn todo_write_enabled(ctx: &ToolStaticContext) -> bool {
    !is_todo_v2_enabled(ctx) && tool_api::todo_tools_enabled(ctx)
}

/// Whether the agent-swarms/teammate surface is live at call time
/// (`isAgentSwarmsEnabled()` / oracle `Jc()`). Gates the `TaskUpdate`
/// auto-owner + owner-change mailbox notification side-effects and the
/// teammate completion reminder.
///
/// Delegates to the SHARED [`lingxi_core::host::env::agent_swarms_enabled`] — one
/// implementation for this crate and `tool-ui`'s SendMessage (the
/// previously-divergent private copies are gone). `isEnabled()` for the swarm
/// *tools* reads the `agent_swarms_enabled` [`ToolStaticContext`] feature
/// flag instead (see `tool_team_create.rs`) — a host-provided surface, not an
/// env read.
#[must_use]
pub fn is_agent_swarms_enabled() -> bool {
    lingxi_core::host::env::agent_swarms_enabled()
}

// ==== Product-A V2 shared helpers ==========================================

/// Wire string for an `lingxi_core::TodoState` (`pending`/`in_progress`/`completed`).
fn status_wire(state: TodoState) -> &'static str {
    match state {
        TodoState::Pending => "pending",
        TodoState::InProgress => "in_progress",
        TodoState::Completed => "completed",
    }
}

/// Render an id list as `#a, #b` (claude-code `ids.map(id => \`#${id}\`)...`).
fn join_hash(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// JS-truthiness of `task.metadata?._internal` (`TaskListTool` filter).
fn metadata_internal_truthy(metadata: &Map<String, Value>) -> bool {
    match metadata.get("_internal") {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true) | Value::Array(_) | Value::Object(_)) => true,
        Some(Value::Number(n)) => n.as_f64().is_none_or(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
    }
}

/// Parsed `status` input for `TaskUpdate`: absent / the special `deleted`
/// action / a real `TodoState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusInput {
    None,
    Deleted,
    State(TodoState),
}

/// Resolve the task-list id — 1:1 port of `getTaskListId()`
/// (`utils/tasks.ts:199-210`). Five-level precedence:
///
/// 1. `LINGXI_TASK_LIST_ID` env (explicit override).
/// 2. In-process teammate `teamName` ([`ToolUseContext::team_name`], TS
///    `getTeammateContext()?.teamName`) — so in-process teammates share the
///    leader's task list.
/// 3. `LINGXI_TEAM_NAME` env (TS `getTeamName()`, set when running as a
///    process-based teammate).
/// 4. Leader team name ([`lingxi_core::host::team_registry::leader_team_name_for_session`], TS
///    `leaderTeamName` set by implicit team initialization).
/// 5. Session id (fallback for standalone sessions).
///
/// The leader and its in-process teammates resolve to the SAME on-disk task dir.
async fn resolve_task_list_id(ctx: &ToolUseContext) -> String {
    // 1. Explicit env override.
    if let Some(explicit) = std::env::var_os("LINGXI_TASK_LIST_ID") {
        if !explicit.is_empty() {
            return explicit.to_string_lossy().into_owned();
        }
    }
    // 2. In-process teammate team name (threaded on the call context).
    if let Some(team) = ctx.team_name.as_deref().filter(|t| !t.is_empty()) {
        return team.to_string();
    }
    // 3. LINGXI_TEAM_NAME env (process-based teammate; TS getTeamName()).
    if let Some(team) = std::env::var_os("LINGXI_TEAM_NAME") {
        if !team.is_empty() {
            return team.to_string_lossy().into_owned();
        }
    }
    // Subagent invocations carry immutable origin identity instead of the
    // mutable main-session handle; both resolve the same host task namespace.
    let session_id = match &ctx.session {
        Some(session) => Some(session.lock().await.session_id.to_string()),
        None => ctx.origin_session_id.map(|id| id.to_string()),
    };
    session_id.map_or_else(
        || "default".to_string(),
        |id| {
            lingxi_core::host::team_registry::leader_team_name_for_session(&id)
                .filter(|team| !team.is_empty())
                .unwrap_or(id)
        },
    )
}

fn todo_store(config_home: Option<&std::path::Path>, list_id: &str) -> TodoStore {
    config_home.map_or_else(
        || TodoStore::for_list(list_id),
        |home| TodoStore::for_list_at(home, list_id),
    )
}

/// Borrowed view of a [`TodoTask`] used to render `TaskList` lines.
struct TaskListRow {
    id: String,
    subject: String,
    status: TodoState,
    owner: Option<String>,
    blocked_by: Vec<String>,
}

/// Render the `TaskList` result string (`mapToolResultToToolResultBlockParam`).
fn render_task_list(rows: &[TaskListRow]) -> String {
    if rows.is_empty() {
        return "No tasks found".to_string();
    }
    rows.iter()
        .map(|row| {
            let owner = row
                .owner
                .as_deref()
                .filter(|o| !o.is_empty())
                .map(|o| format!(" ({o})"))
                .unwrap_or_default();
            let blocked = if row.blocked_by.is_empty() {
                String::new()
            } else {
                format!(" [blocked by {}]", join_hash(&row.blocked_by))
            };
            format!(
                "#{} [{}] {}{owner}{blocked}",
                row.id,
                status_wire(row.status),
                row.subject
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render the `TaskGet` result string.
fn render_task_get(task: Option<&TodoTask>) -> String {
    let Some(task) = task else {
        return "Task not found".to_string();
    };
    let mut lines = vec![
        format!("Task #{}: {}", task.id, task.subject),
        format!("Status: {}", status_wire(task.status)),
        format!("Description: {}", task.description),
    ];
    if !task.blocked_by.is_empty() {
        lines.push(format!("Blocked by: {}", join_hash(&task.blocked_by)));
    }
    if !task.blocks.is_empty() {
        lines.push(format!("Blocks: {}", join_hash(&task.blocks)));
    }
    lines.join("\n")
}

async fn emit_started(
    bus: &Arc<AnalyticsBus>,
    event: &'static str,
    invocation_id: &str,
    extras: &[(&'static str, AnalyticsValue)],
) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "invocation_id".into(),
        AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
    );
    for (k, v) in extras {
        md.insert((*k).into(), v.clone());
    }
    bus.log_event(event, md).await;
}

async fn emit_completed(
    bus: &Arc<AnalyticsBus>,
    event: &'static str,
    invocation_id: &str,
    duration_ms: u64,
    extras: &[(&'static str, AnalyticsValue)],
) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "invocation_id".into(),
        AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
    );
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    for (k, v) in extras {
        md.insert((*k).into(), v.clone());
    }
    bus.log_event(event, md).await;
}

/// Convert a `TaskRegistryError` to a `ToolError` so the 6 task tools can
/// share one mapping.
fn registry_err_to_tool_err(prefix: &str, e: TaskRegistryError) -> ToolError {
    match e {
        TaskRegistryError::NotFound(id) => {
            ToolError::InvalidInput(format!("{prefix}: task not found: {id}"))
        }
        TaskRegistryError::InvalidInput(s) => ToolError::InvalidInput(format!("{prefix}: {s}")),
        TaskRegistryError::Internal(s) => ToolError::Internal(format!("{prefix}: {s}")),
    }
}

async fn emit_failed(
    bus: &Arc<AnalyticsBus>,
    event: &'static str,
    invocation_id: &str,
    error_kind: &str,
    duration_ms: u64,
) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "invocation_id".into(),
        AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
    );
    md.insert(
        "error_kind".into(),
        AnalyticsValue::String(Verified::assert_safe(error_kind.to_string()).into_inner()),
    );
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(event, md).await;
}

// ==== Product-A V2 prompts (verbatim base + swarms-enabled variants) ====

const TASK_CREATE_PROMPT: &str = r#"Use this tool to create a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.
It also helps the user understand the progress of the task and overall progress of their requests.

## When to Use This Tool

Use this tool proactively in these scenarios:

- Complex multi-step tasks - When a task requires 3 or more distinct steps or actions
- Non-trivial and complex tasks - Tasks that require careful planning or multiple operations
- Plan mode - When using plan mode, create a task list to track the work
- User explicitly requests todo list - When the user directly asks you to use the todo list
- User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated)
- After receiving new instructions - Immediately capture user requirements as tasks
- When you start working on a task - Mark it as in_progress BEFORE beginning work
- After completing a task - Mark it as completed and add any new follow-up tasks discovered during implementation

## When NOT to Use This Tool

Skip using this tool when:
- There is only a single, straightforward task
- The task is trivial and tracking it provides no organizational benefit
- The task can be completed in less than 3 trivial steps
- The task is purely conversational or informational

NOTE that you should not use this tool if there is only one trivial task to do. In this case you are better off just doing the task directly.

## Task Fields

- **subject**: A brief, actionable title in imperative form (e.g., "Fix authentication bug in login flow")
- **description**: What needs to be done
- **activeForm** (optional): Present continuous form shown in the spinner when the task is in_progress (e.g., "Fixing authentication bug"). If omitted, the spinner shows the subject instead.

All tasks are created with status `pending`.

## Tips

- Create tasks with clear, specific subjects that describe the outcome
- After creating tasks, use TaskUpdate to set up dependencies (blocks/blockedBy) if needed
- Check TaskList first to avoid creating duplicate tasks
"#;

/// `TaskCreate` prompt — swarms-ENABLED variant (`TaskCreateTool/prompt.ts`
/// `getPrompt()` with `isAgentSwarmsEnabled() === true`). Splices
/// `teammateContext` ("and potentially assigned to teammates") onto the
/// "multiple operations" bullet and inserts the two `teammateTips` bullets
/// before the final "Check TaskList first" tip. Byte-identical to the TS output.
const TASK_CREATE_PROMPT_SWARM: &str = r#"Use this tool to create a structured task list for your current coding session. This helps you track progress, organize complex tasks, and demonstrate thoroughness to the user.
It also helps the user understand the progress of the task and overall progress of their requests.

## When to Use This Tool

Use this tool proactively in these scenarios:

- Complex multi-step tasks - When a task requires 3 or more distinct steps or actions
- Non-trivial and complex tasks - Tasks that require careful planning or multiple operations and potentially assigned to teammates
- Plan mode - When using plan mode, create a task list to track the work
- User explicitly requests todo list - When the user directly asks you to use the todo list
- User provides multiple tasks - When users provide a list of things to be done (numbered or comma-separated)
- After receiving new instructions - Immediately capture user requirements as tasks
- When you start working on a task - Mark it as in_progress BEFORE beginning work
- After completing a task - Mark it as completed and add any new follow-up tasks discovered during implementation

## When NOT to Use This Tool

Skip using this tool when:
- There is only a single, straightforward task
- The task is trivial and tracking it provides no organizational benefit
- The task can be completed in less than 3 trivial steps
- The task is purely conversational or informational

NOTE that you should not use this tool if there is only one trivial task to do. In this case you are better off just doing the task directly.

## Task Fields

- **subject**: A brief, actionable title in imperative form (e.g., "Fix authentication bug in login flow")
- **description**: What needs to be done
- **activeForm** (optional): Present continuous form shown in the spinner when the task is in_progress (e.g., "Fixing authentication bug"). If omitted, the spinner shows the subject instead.

All tasks are created with status `pending`.

## Tips

- Create tasks with clear, specific subjects that describe the outcome
- After creating tasks, use TaskUpdate to set up dependencies (blocks/blockedBy) if needed
- Include enough detail in the description for another agent to understand and complete the task
- New tasks are created with status 'pending' and no owner - use TaskUpdate with the `owner` parameter to assign them
- Check TaskList first to avoid creating duplicate tasks
"#;

const TASK_GET_PROMPT: &str = r"Use this tool to retrieve a task by its ID from the task list.

## When to Use This Tool

- When you need the full description and context before starting work on a task
- To understand task dependencies (what it blocks, what blocks it)
- After being assigned a task, to get complete requirements

## Output

Returns full task details:
- **subject**: Task title
- **description**: Detailed requirements and context
- **status**: 'pending', 'in_progress', or 'completed'
- **blocks**: Tasks waiting on this one to complete
- **blockedBy**: Tasks that must complete before this one can start

## Tips

- After fetching a task, verify its blockedBy list is empty before beginning work.
- Use TaskList to see all tasks in summary form.
";

const TASK_LIST_PROMPT: &str = r"Use this tool to list all tasks in the task list.

## When to Use This Tool

- To see what tasks are available to work on (status: 'pending', no owner, not blocked)
- To check overall progress on the project
- To find tasks that are blocked and need dependencies resolved
- After completing a task, to check for newly unblocked work or claim the next available task
- **Prefer working on tasks in ID order** (lowest ID first) when multiple tasks are available, as earlier tasks often set up context for later ones

## Output

Returns a summary of each task:
- **id**: Task identifier (use with TaskGet, TaskUpdate)
- **subject**: Brief description of the task
- **status**: 'pending', 'in_progress', or 'completed'
- **owner**: Agent ID if assigned, empty if available
- **blockedBy**: List of open task IDs that must be resolved first (tasks with blockedBy cannot be claimed until dependencies resolve)

Use TaskGet with a specific task ID to view full details including description and comments.
";

/// `TaskList` prompt — swarms-ENABLED variant (`TaskListTool/prompt.ts`
/// `getPrompt()` with `isAgentSwarmsEnabled() === true`). Adds the
/// `teammateUseCase` bullet after the "need dependencies resolved" line and
/// appends the `## Teammate Workflow` section. Byte-identical to the TS output.
const TASK_LIST_PROMPT_SWARM: &str = r"Use this tool to list all tasks in the task list.

## When to Use This Tool

- To see what tasks are available to work on (status: 'pending', no owner, not blocked)
- To check overall progress on the project
- To find tasks that are blocked and need dependencies resolved
- Before assigning tasks to teammates, to see what's available
- After completing a task, to check for newly unblocked work or claim the next available task
- **Prefer working on tasks in ID order** (lowest ID first) when multiple tasks are available, as earlier tasks often set up context for later ones

## Output

Returns a summary of each task:
- **id**: Task identifier (use with TaskGet, TaskUpdate)
- **subject**: Brief description of the task
- **status**: 'pending', 'in_progress', or 'completed'
- **owner**: Agent ID if assigned, empty if available
- **blockedBy**: List of open task IDs that must be resolved first (tasks with blockedBy cannot be claimed until dependencies resolve)

Use TaskGet with a specific task ID to view full details including description and comments.

## Teammate Workflow

When working as a teammate:
1. After completing your current task, call TaskList to find available work
2. Look for tasks with status 'pending', no owner, and empty blockedBy
3. **Prefer tasks in ID order** (lowest ID first) when multiple tasks are available, as earlier tasks often set up context for later ones
4. Claim an available task using TaskUpdate (set `owner` to your name), or wait for leader assignment
5. If blocked, focus on unblocking tasks or notify the team lead
";

const TASK_UPDATE_PROMPT: &str = r#"Use this tool to update a task in the task list.

## When to Use This Tool

**Mark tasks as resolved:**
- When you have completed the work described in a task
- When a task is no longer needed or has been superseded
- IMPORTANT: Always mark your assigned tasks as resolved when you finish them
- After resolving, call TaskList to find your next task

- ONLY mark a task as completed when you have FULLY accomplished it
- If you encounter errors, blockers, or cannot finish, keep the task as in_progress
- When blocked, create a new task describing what needs to be resolved
- Never mark a task as completed if:
  - Tests are failing
  - Implementation is partial
  - You encountered unresolved errors
  - You couldn't find necessary files or dependencies

**Delete tasks:**
- When a task is no longer relevant or was created in error
- Setting status to `deleted` permanently removes the task

**Update task details:**
- When requirements change or become clearer
- When establishing dependencies between tasks

## Fields You Can Update

- **status**: The task status (see Status Workflow below)
- **subject**: Change the task title (imperative form, e.g., "Run tests")
- **description**: Change the task description
- **activeForm**: Present continuous form shown in spinner when in_progress (e.g., "Running tests")
- **owner**: Change the task owner (agent name)
- **metadata**: Merge metadata keys into the task (set a key to null to delete it)
- **addBlocks**: Mark tasks that cannot start until this one completes
- **addBlockedBy**: Mark tasks that must complete before this one can start

## Status Workflow

Status progresses: `pending` → `in_progress` → `completed`

Use `deleted` to permanently remove a task.

## Staleness

Make sure to read a task's latest state using `TaskGet` before updating it.

## Examples

Mark task as in progress when starting work:
```json
{"taskId": "1", "status": "in_progress"}
```

Mark task as completed after finishing work:
```json
{"taskId": "1", "status": "completed"}
```

Delete a task:
```json
{"taskId": "1", "status": "deleted"}
```

Claim a task by setting owner:
```json
{"taskId": "1", "owner": "my-name"}
```

Set up task dependencies:
```json
{"taskId": "2", "addBlockedBy": ["1"]}
```
"#;

// ==== TaskCreateTool ========================================================

static TASK_CREATE_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["subject", "description"],
        "properties": {
            "subject":     { "type": "string", "description": "A brief title for the task" },
            "description": { "type": "string", "description": "What needs to be done" },
            "activeForm":  { "type": "string", "description": "Present continuous form shown in spinner when in_progress (e.g., \"Running tests\")" },
            "metadata":    { "type": "object", "description": "Arbitrary metadata to attach to the task" }
        }
    })
});

// ── Input coercion (claude-code `POe` / `Cce`) and steering (`yDn`) ──────
//
// The model reliably reaches for TaskCreate with the shapes it uses for
// neighbouring tools — a `tasks` array, a `task` wrapper object, Agent-tool
// `prompt`/`subagent_type`, or `title`/`content` instead of
// `subject`/`description`. Upstream repairs what it can and STEERS what it
// cannot, so a near-miss becomes a working call or a sentence telling the model
// what to do instead. Without them the call just fails the schema.
//
// `shape_class` is the diagnostic label upstream attaches to a repaired call;
// its tags are emitted in the order the repairs were applied. This workspace
// builds `serde_json` with `preserve_order`, so key iteration matches the JS
// object order the strip pass walks.

/// `OR(e)` — a string with non-whitespace content.
fn is_filled_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// `AOe(e)` — carries a batch parameter TaskCreate does not have.
fn has_batch_key(map: &serde_json::Map<String, Value>) -> bool {
    map.contains_key("tasks") || map.contains_key("todos")
}

/// `ROe(e)` — carries Agent-tool parameters.
fn has_agent_keys(map: &serde_json::Map<String, Value>) -> bool {
    map.contains_key("prompt") || map.contains_key("subagent_type")
}

/// `TOo(e)` — derive a subject from a description: trim, cap at 80 CHARACTERS
/// (`Array.from`, i.e. code points), and prefer to cut on the last space when
/// that space is past the halfway point.
fn subject_from_description(description: &str) -> String {
    let trimmed = description.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 80 {
        return trimmed.to_string();
    }
    let head: String = chars[..80].iter().collect();
    match head.rfind(' ') {
        // `d>40` is a BYTE index upstream, but the comparison is against a
        // fixed 40 and the cut is the same character boundary either way for
        // any input where a space exists in the first 80 characters.
        Some(idx) if idx > 40 => head[..idx].trim().to_string(),
        _ => head.trim().to_string(),
    }
}

/// Aliases upstream accepts for each canonical field.
const SUBJECT_ALIASES: [&str; 2] = ["title", "name"];
const DESCRIPTION_ALIASES: [&str; 1] = ["content"];
const ACTIVE_FORM_ALIASES: [&str; 1] = ["active_form"];
/// `bOo` — the only keys a repaired TaskCreate call may keep.
const TASK_CREATE_KEEP: [&str; 4] = ["subject", "description", "activeForm", "metadata"];
/// `EOo` — keys named individually in the strip tag; anything else is `other`.
const TASK_CREATE_KNOWN_STRIP: [&str; 12] = [
    "status",
    "state",
    "priority",
    "prompt",
    "subagent_type",
    "id",
    "type",
    "owner",
    "blocks",
    "blockedBy",
    "addBlocks",
    "addBlockedBy",
];

/// Move `alias` onto `target` when the target is absent and the alias holds a
/// filled string. Returns the tag to record.
fn apply_alias(
    map: &mut serde_json::Map<String, Value>,
    aliases: &[&str],
    target: &str,
    tags: &mut Vec<String>,
) {
    for alias in aliases {
        if map.contains_key(*alias)
            && !map.contains_key(target)
            && is_filled_string(map.get(*alias))
        {
            if let Some(value) = map.remove(*alias) {
                map.insert(target.to_string(), value);
                tags.push(format!("alias_{alias}"));
            }
        }
    }
}

/// `POe` — repair a near-miss TaskCreate call, or `None` when there is nothing
/// to repair (or the shape is one `yDn` steers instead).
fn coerce_task_create_input(input: &Value) -> Option<CoercedInput> {
    let map = input.as_object()?;
    if has_batch_key(map) {
        // Not repairable — `yDn` explains it instead.
        return None;
    }
    let mut out = map.clone();
    let mut tags: Vec<String> = Vec::new();
    if has_agent_keys(&out)
        && !(is_filled_string(out.get("subject")) && is_filled_string(out.get("description")))
    {
        return None;
    }
    // A `task` wrapper: either the description as a bare string, or the real
    // arguments one level down.
    if !out.contains_key("subject") && !out.contains_key("description") && out.contains_key("task")
    {
        let wrapped = out.get("task").cloned()?;
        if is_filled_string(Some(&wrapped)) {
            out.remove("task");
            out.insert("description".into(), wrapped);
            tags.push("task_wrapper_string".into());
        } else if let Some(inner) = wrapped.as_object() {
            if has_batch_key(inner) {
                return None;
            }
            if has_agent_keys(inner)
                && !(is_filled_string(inner.get("subject"))
                    && is_filled_string(inner.get("description")))
            {
                return None;
            }
            out.remove("task");
            for (key, value) in inner.clone() {
                out.insert(key, value);
            }
            tags.push("task_wrapper_object".into());
        } else {
            return None;
        }
    }
    apply_alias(&mut out, &SUBJECT_ALIASES, "subject", &mut tags);
    apply_alias(&mut out, &DESCRIPTION_ALIASES, "description", &mut tags);
    apply_alias(&mut out, &ACTIVE_FORM_ALIASES, "activeForm", &mut tags);

    if is_filled_string(out.get("subject")) && !out.contains_key("description") {
        let subject = out.get("subject").cloned().unwrap_or(Value::Null);
        out.insert("description".into(), subject);
        tags.push("backfill_description".into());
    } else if is_filled_string(out.get("description")) && !out.contains_key("subject") {
        let derived = out
            .get("description")
            .and_then(Value::as_str)
            .map(subject_from_description)
            .unwrap_or_default();
        out.insert("subject".into(), Value::String(derived));
        tags.push("backfill_subject".into());
    }

    // Only once both required fields are present is it safe to drop the rest.
    if is_filled_string(out.get("subject")) && is_filled_string(out.get("description")) {
        let strip: Vec<String> = out
            .keys()
            .filter(|key| !TASK_CREATE_KEEP.contains(&key.as_str()))
            .cloned()
            .collect();
        for key in strip {
            out.remove(&key);
            let label = if TASK_CREATE_KNOWN_STRIP.contains(&key.as_str()) {
                key
            } else {
                "other".to_string()
            };
            tags.push(format!("strip_{label}"));
        }
        if out.get("activeForm").is_some_and(|v| !v.is_string()) {
            out.remove("activeForm");
            tags.push("drop_invalid_activeForm".into());
        }
        if out.get("metadata").is_some_and(|v| !v.is_object()) {
            out.remove("metadata");
            tags.push("drop_invalid_metadata".into());
        }
    }
    if tags.is_empty() {
        return None;
    }
    Some(CoercedInput {
        input: Value::Object(out),
        shape_class: tags.join("+"),
    })
}

/// `yDn` — the sentence shown instead of a bare schema rejection.
fn task_create_steer(input: &Value) -> Option<&'static str> {
    let map = input.as_object()?;
    let inner = map.get("task").and_then(Value::as_object);
    if has_batch_key(map) || inner.is_some_and(has_batch_key) {
        return Some(
            "TaskCreate creates ONE task per call and has no `tasks` or `todos` parameter. Call TaskCreate once per task, passing `subject` (a brief title) and `description` (what needs to be done) as top-level string parameters.",
        );
    }
    if (has_agent_keys(map) || inner.is_some_and(has_agent_keys))
        && !(is_filled_string(map.get("subject")) && is_filled_string(map.get("description")))
    {
        return Some(
            "This call used Agent-tool parameters (`prompt`/`subagent_type`). TaskCreate adds an item to the task list and takes `subject` and `description` string parameters. To delegate work to a subagent, use the Agent tool instead.",
        );
    }
    None
}

/// `Cce` — TaskUpdate accepts `id`/`task_id` for `taskId` and `active_form`
/// for `activeForm`.
fn coerce_task_update_input(input: &Value) -> Option<CoercedInput> {
    let map = input.as_object()?;
    let mut out = map.clone();
    let mut tags: Vec<String> = Vec::new();
    apply_alias(&mut out, &["id", "task_id"], "taskId", &mut tags);
    apply_alias(&mut out, &ACTIVE_FORM_ALIASES, "activeForm", &mut tags);
    if tags.is_empty() {
        return None;
    }
    Some(CoercedInput {
        input: Value::Object(out),
        shape_class: tags.join("+"),
    })
}

/// Product-A V2 `TaskCreate` — appends a task to the todo store.
pub struct TaskCreateTool {
    ctx: BuiltinToolContext,
    config_home: Option<std::path::PathBuf>,
}

impl TaskCreateTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            config_home: None,
        }
    }

    /// Pin task persistence to a host-resolved config home.
    #[must_use]
    pub fn with_config_home(mut self, config_home: std::path::PathBuf) -> Self {
        self.config_home = Some(config_home);
        self
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        TASK_CREATE_TOOL_NAME
    }
    fn search_hint(&self) -> Option<&str> {
        Some("create a task in the task list")
    }
    fn input_schema(&self) -> &Value {
        &TASK_CREATE_SCHEMA
    }
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool {
        task_tools_enabled(ctx)
    }
    /// claude-code `coerceInput: POe` — repair a near-miss call rather than
    /// bouncing it off the schema.
    fn coerce_input(&self, input: &Value) -> Option<CoercedInput> {
        coerce_task_create_input(input)
    }
    /// claude-code `validationErrorSteer: yDn` — the shapes coercion cannot
    /// repair get a sentence saying what to call instead. Both are shapes this
    /// tool's schema rejects anyway (it is `additionalProperties: false`, the
    /// port of upstream's strict object), so this replaces an opaque rejection
    /// rather than adding one.
    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        match task_create_steer(input) {
            Some(steer) => Err(ValidationError(steer.to_string())),
            None => Ok(()),
        }
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    // claude TaskCreate `isConcurrencySafe(){return!1}` — TaskCreate mutates the
    // shared task list, so it must NOT be batched concurrently with other tools.
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "TaskCreate appends a task to the session task list".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Create a new task in the task list".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        // getPrompt() (TaskCreateTool/prompt.ts): the swarms-ENABLED variant
        // splices the teammateContext / teammateTips inserts; the disabled
        // variant is byte-identical to the base const.
        if is_agent_swarms_enabled() {
            TASK_CREATE_PROMPT_SWARM.into()
        } else {
            TASK_CREATE_PROMPT.into()
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = fresh_invocation_id();
        let bus = self.ctx.bus.clone();

        let subject = match input.get("subject").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(
                    &bus,
                    TASK_CREATE_FAILED,
                    &invocation_id,
                    "missing_subject",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    "TaskCreate: missing 'subject'".into(),
                ));
            }
        };
        let description = match input.get("description").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(
                    &bus,
                    TASK_CREATE_FAILED,
                    &invocation_id,
                    "missing_description",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    "TaskCreate: missing 'description'".into(),
                ));
            }
        };
        let active_form = input
            .get("activeForm")
            .and_then(Value::as_str)
            .map(str::to_string);
        let metadata = input
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        emit_started(&bus, TASK_CREATE_STARTED, &invocation_id, &[]).await;

        let list_id = resolve_task_list_id(&ctx).await;
        let store = todo_store(self.config_home.as_deref(), &list_id);
        let task = TodoTask::new(subject.clone(), description.clone(), active_form, metadata);
        let task_id = match store.create(task).await {
            Ok(id) => id,
            Err(e) => {
                emit_failed(
                    &bus,
                    TASK_CREATE_FAILED,
                    &invocation_id,
                    "store_error",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::Internal(format!("TaskCreate: {e}")));
            }
        };

        // BLOCKING TaskCreated hooks (TaskCreateTool.ts:93-113). After the task
        // is persisted, fire the `TaskCreated` hook through the tool-path firer
        // (when one is wired). On a blocking error claude-code deletes the
        // just-created task and throws — so we roll the store entry back and
        // surface the block as a tool error. NOT swarm-gated: TS fires
        // `executeTaskCreatedHooks` unconditionally (the hook runs whenever a
        // `TaskCreated` hook is registered), so we do too. With no firer wired
        // (`None`) this is a no-op and creation proceeds — matching the
        // fire-and-forget registry path.
        if let Some(firer) = self.ctx.task_lifecycle_hooks.as_ref() {
            if let Err(reason) = firer
                .fire_task_created(
                    &task_id,
                    &subject,
                    Some(description.as_str()),
                    // claude-code `getAgentName()` / `getTeamName()`
                    // (`TaskCreateTool.ts:97-98`) — the creating teammate's
                    // display name + team, threaded on the call context.
                    ctx.agent_name.as_deref(),
                    ctx.team_name.as_deref(),
                )
                .await
            {
                // TS `await deleteTask(getTaskListId(), taskId)` then `throw`.
                store.delete(&task_id).await;
                emit_failed(
                    &bus,
                    TASK_CREATE_FAILED,
                    &invocation_id,
                    "blocked_by_hook",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                // T24: each blocking error is wrapped by `getTaskCreatedHookMessage`
                // (`hooks.ts:1914-1917`) as `TaskCreated hook feedback:\n<error>`,
                // then the wrapped messages are `\n`-joined and thrown
                // (`TaskCreateTool.ts:106,112`). The firer aggregates the (possibly
                // several) blocking hooks into one `\n`-joined reason, so the prefix
                // applies once to the aggregate — byte-faithful with the TS feedback
                // the model sees.
                return Err(ToolError::Internal(format!(
                    "TaskCreated hook feedback:\n{reason}"
                )));
            }
        }
        // The tool layer stays renderer-neutral. The TUI consumes this successful
        // TaskCreate result in `apply_plan_tool_result` and updates the expanded
        // planned-task view above the composer.

        emit_completed(
            &bus,
            TASK_CREATE_COMPLETED,
            &invocation_id,
            started.elapsed().as_millis() as u64,
            &[(
                "task_id",
                AnalyticsValue::String(Verified::assert_safe(task_id.clone()).into_inner()),
            )],
        )
        .await;

        Ok(ToolCallResult {
            data: json!({
                "content": format!("Task #{task_id} created successfully: {subject}"),
                "task": { "id": task_id, "subject": subject },
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ==== TaskGetTool ============================================================

static TASK_GET_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["taskId"],
        "properties": {
            "taskId": { "type": "string", "description": "The ID of the task to retrieve" }
        }
    })
});

/// Product-A V2 `TaskGet` — read one task from the todo store.
pub struct TaskGetTool {
    ctx: BuiltinToolContext,
    config_home: Option<std::path::PathBuf>,
}

impl TaskGetTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            config_home: None,
        }
    }

    /// Pin task persistence to a host-resolved config home.
    #[must_use]
    pub fn with_config_home(mut self, config_home: std::path::PathBuf) -> Self {
        self.config_home = Some(config_home);
        self
    }
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        TASK_GET_TOOL_NAME
    }
    fn search_hint(&self) -> Option<&str> {
        Some("retrieve a task by ID")
    }
    fn input_schema(&self) -> &Value {
        &TASK_GET_SCHEMA
    }
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool {
        task_tools_enabled(ctx)
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "TaskGet is read-only".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Get a task by ID from the task list".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        TASK_GET_PROMPT.into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = fresh_invocation_id();
        let bus = self.ctx.bus.clone();

        let task_id = match input.get("taskId").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(
                    &bus,
                    TASK_GET_FAILED,
                    &invocation_id,
                    "missing_task_id",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput("TaskGet: missing 'taskId'".into()));
            }
        };

        emit_started(&bus, TASK_GET_STARTED, &invocation_id, &[]).await;

        let list_id = resolve_task_list_id(&ctx).await;
        let store = todo_store(self.config_home.as_deref(), &list_id);
        let task = store.get(&task_id).await;

        emit_completed(
            &bus,
            TASK_GET_COMPLETED,
            &invocation_id,
            started.elapsed().as_millis() as u64,
            &[],
        )
        .await;

        let content = render_task_get(task.as_ref());
        let task_json = match &task {
            Some(t) => json!({
                "id": t.id,
                "subject": t.subject,
                "description": t.description,
                "status": status_wire(t.status),
                "blocks": t.blocks,
                "blockedBy": t.blocked_by,
            }),
            None => Value::Null,
        };
        Ok(ToolCallResult {
            data: json!({ "content": content, "task": task_json }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ==== TaskListTool ===========================================================

static TASK_LIST_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    })
});

/// Product-A V2 `TaskList` — enumerate the todo store.
pub struct TaskListTool {
    ctx: BuiltinToolContext,
    config_home: Option<std::path::PathBuf>,
}

impl TaskListTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            config_home: None,
        }
    }

    /// Pin task persistence to a host-resolved config home.
    #[must_use]
    pub fn with_config_home(mut self, config_home: std::path::PathBuf) -> Self {
        self.config_home = Some(config_home);
        self
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        TASK_LIST_TOOL_NAME
    }
    fn search_hint(&self) -> Option<&str> {
        Some("list all tasks")
    }
    fn input_schema(&self) -> &Value {
        &TASK_LIST_SCHEMA
    }
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool {
        task_tools_enabled(ctx)
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "TaskList is read-only".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "List all tasks in the task list".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        // getPrompt() (TaskListTool/prompt.ts): the swarms-ENABLED variant adds
        // the teammate use-case bullet + `## Teammate Workflow` section; the
        // disabled variant is byte-identical to the base const.
        if is_agent_swarms_enabled() {
            TASK_LIST_PROMPT_SWARM.into()
        } else {
            TASK_LIST_PROMPT.into()
        }
    }

    async fn call(
        &self,
        _input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = fresh_invocation_id();
        let bus = self.ctx.bus.clone();

        emit_started(&bus, TASK_LIST_STARTED, &invocation_id, &[]).await;

        let list_id = resolve_task_list_id(&ctx).await;
        let store = todo_store(self.config_home.as_deref(), &list_id);
        // Filter out tasks flagged metadata._internal (TaskListTool.ts:435-437).
        let all: Vec<TodoTask> = store
            .list()
            .await
            .into_iter()
            .filter(|t| !metadata_internal_truthy(&t.metadata))
            .collect();

        emit_completed(
            &bus,
            TASK_LIST_COMPLETED,
            &invocation_id,
            started.elapsed().as_millis() as u64,
            &[("count", AnalyticsValue::Int(all.len() as i64))],
        )
        .await;

        // blockedBy is recomputed to drop already-completed blockers.
        let resolved: HashSet<&str> = all
            .iter()
            .filter(|t| t.status == TodoState::Completed)
            .map(|t| t.id.as_str())
            .collect();

        let rows: Vec<TaskListRow> = all
            .iter()
            .map(|t| TaskListRow {
                id: t.id.clone(),
                subject: t.subject.clone(),
                status: t.status,
                owner: t.owner.clone(),
                blocked_by: t
                    .blocked_by
                    .iter()
                    .filter(|id| !resolved.contains(id.as_str()))
                    .cloned()
                    .collect(),
            })
            .collect();

        let content = render_task_list(&rows);
        let tasks: Vec<Value> = rows
            .iter()
            .map(|row| {
                // T27: `owner` is `z.string().optional()` (TaskListTool.ts:23) and
                // sourced from `task.owner` (`call`, line 80), which is `undefined`
                // when unset — so the key is OMITTED from the JSON, never emitted
                // as `owner: null`. Build the object and insert `owner` only when
                // present.
                let mut obj = json!({
                    "id": row.id,
                    "subject": row.subject,
                    "status": status_wire(row.status),
                    "blockedBy": row.blocked_by,
                });
                if let Some(owner) = row.owner.as_deref() {
                    obj["owner"] = Value::String(owner.to_string());
                }
                obj
            })
            .collect();
        Ok(ToolCallResult {
            data: json!({ "content": content, "tasks": tasks }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ==== TaskUpdateTool =========================================================

static TASK_UPDATE_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["taskId"],
        "properties": {
            "taskId":       { "type": "string", "description": "The ID of the task to update" },
            "subject":      { "type": "string", "description": "New subject for the task" },
            "description":  { "type": "string", "description": "New description for the task" },
            "activeForm":   { "type": "string", "description": "Present continuous form shown in spinner when in_progress (e.g., \"Running tests\")" },
            "status":       { "type": "string", "enum": ["pending","in_progress","completed","deleted"], "description": "New status for the task" },
            "addBlocks":    { "type": "array", "items": { "type": "string" }, "description": "Task IDs that this task blocks" },
            "addBlockedBy": { "type": "array", "items": { "type": "string" }, "description": "Task IDs that block this task" },
            "owner":        { "type": "string", "description": "New owner for the task" },
            "metadata":     { "type": "object", "description": "Metadata keys to merge into the task. Set a key to null to delete it." }
        }
    })
});

/// Render the `TaskUpdate` success string (`Updated task #id field, field`).
fn render_task_update_success(task_id: &str, updated_fields: &[String]) -> String {
    format!("Updated task #{task_id} {}", updated_fields.join(", "))
}

/// Render the `TaskUpdate` failure string (`error || "Task #id not found"`).
fn render_task_update_fail(task_id: &str, error: Option<&str>) -> String {
    match error.filter(|e| !e.is_empty()) {
        Some(e) => e.to_string(),
        None => format!("Task #{task_id} not found"),
    }
}

/// Product-A V2 `TaskUpdate` — mutate a task in the todo store.
pub struct TaskUpdateTool {
    ctx: BuiltinToolContext,
    config_home: Option<std::path::PathBuf>,
}

impl TaskUpdateTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            config_home: None,
        }
    }

    /// Pin task persistence to a host-resolved config home.
    #[must_use]
    pub fn with_config_home(mut self, config_home: std::path::PathBuf) -> Self {
        self.config_home = Some(config_home);
        self
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        TASK_UPDATE_TOOL_NAME
    }
    fn search_hint(&self) -> Option<&str> {
        Some("update a task")
    }
    fn input_schema(&self) -> &Value {
        &TASK_UPDATE_SCHEMA
    }
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool {
        task_tools_enabled(ctx)
    }
    /// claude-code `coerceInput: Cce` — TaskUpdate accepts `id`/`task_id` for
    /// `taskId` and `active_form` for `activeForm`.
    fn coerce_input(&self, input: &Value) -> Option<CoercedInput> {
        coerce_task_update_input(input)
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "TaskUpdate mutates the session task list only".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Update a task in the task list".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        TASK_UPDATE_PROMPT.into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = fresh_invocation_id();
        let bus = self.ctx.bus.clone();

        let task_id = match input.get("taskId").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(
                    &bus,
                    TASK_UPDATE_FAILED,
                    &invocation_id,
                    "missing_task_id",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    "TaskUpdate: missing 'taskId'".into(),
                ));
            }
        };

        emit_started(&bus, TASK_UPDATE_STARTED, &invocation_id, &[]).await;
        let duration = || started.elapsed().as_millis() as u64;

        let list_id = resolve_task_list_id(&ctx).await;
        let store = todo_store(self.config_home.as_deref(), &list_id);

        let existing = match store.get(&task_id).await {
            Some(t) => t,
            None => {
                emit_completed(&bus, TASK_UPDATE_COMPLETED, &invocation_id, duration(), &[]).await;
                // Oracle data shape (2.1.223 DCb): no `content` key — the
                // model-facing string is rendered by
                // mapToolResultToToolResultBlockParam (`error || "Task #N not
                // found"`), carried here via `model_content`.
                return Ok(ToolCallResult {
                    data: json!({
                        "success": false,
                        "taskId": task_id,
                        "updatedFields": Vec::<String>::new(),
                        "error": "Task not found",
                    }),
                    model_content: Some(render_task_update_fail(&task_id, Some("Task not found"))),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                });
            }
        };

        // Status: None / "deleted" (special action) / a real TodoState.
        let status_input = match input.get("status").and_then(Value::as_str) {
            None => StatusInput::None,
            Some("deleted") => StatusInput::Deleted,
            Some("pending") => StatusInput::State(TodoState::Pending),
            Some("in_progress") => StatusInput::State(TodoState::InProgress),
            Some("completed") => StatusInput::State(TodoState::Completed),
            Some(other) => {
                emit_failed(
                    &bus,
                    TASK_UPDATE_FAILED,
                    &invocation_id,
                    "invalid_status",
                    duration(),
                )
                .await;
                return Err(ToolError::InvalidInput(format!(
                    "TaskUpdate: invalid status '{other}'"
                )));
            }
        };

        // status == "deleted" → delete + early return (TaskUpdateTool.ts:755-768).
        if let StatusInput::Deleted = status_input {
            let deleted = store.delete(&task_id).await;
            emit_completed(&bus, TASK_UPDATE_COMPLETED, &invocation_id, duration(), &[]).await;
            let (data, model_content) = if deleted {
                (
                    json!({
                        "success": true,
                        "taskId": task_id,
                        "updatedFields": ["deleted"],
                        "statusChange": { "from": status_wire(existing.status), "to": "deleted" },
                    }),
                    render_task_update_success(&task_id, &["deleted".to_string()]),
                )
            } else {
                (
                    json!({
                        "success": false,
                        "taskId": task_id,
                        "updatedFields": Vec::<String>::new(),
                        "error": "Failed to delete task",
                    }),
                    render_task_update_fail(&task_id, Some("Failed to delete task")),
                )
            };
            return Ok(ToolCallResult {
                data,
                model_content: Some(model_content),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            });
        }

        // Diff each provided field against the existing task.
        let in_subject = input.get("subject").and_then(Value::as_str);
        let in_description = input.get("description").and_then(Value::as_str);
        let in_active_form = input.get("activeForm").and_then(Value::as_str);
        let in_owner = input.get("owner").and_then(Value::as_str);
        let in_metadata = input.get("metadata").and_then(Value::as_object);

        let mut updated_fields: Vec<String> = Vec::new();
        let mut new_subject: Option<String> = None;
        let mut new_description: Option<String> = None;
        let mut new_active_form: Option<String> = None;
        let mut new_owner: Option<String> = None;
        let mut new_status: Option<TodoState> = None;
        let mut new_metadata: Option<Map<String, Value>> = None;

        if let Some(s) = in_subject {
            if s != existing.subject {
                new_subject = Some(s.to_string());
                updated_fields.push("subject".into());
            }
        }
        if let Some(d) = in_description {
            if d != existing.description {
                new_description = Some(d.to_string());
                updated_fields.push("description".into());
            }
        }
        if let Some(af) = in_active_form {
            if Some(af) != existing.active_form.as_deref() {
                new_active_form = Some(af.to_string());
                updated_fields.push("activeForm".into());
            }
        }
        if let Some(o) = in_owner {
            if Some(o) != existing.owner.as_deref() {
                new_owner = Some(o.to_string());
                updated_fields.push("owner".into());
            }
        }
        // Auto-set owner when a teammate marks a task as in_progress without
        // explicitly providing an owner. This ensures the task list can match
        // todo items to teammates for showing activity status
        // (TaskUpdateTool.ts:188-199). claude-code uses `getAgentName()` — the
        // teammate's DISPLAY NAME ([`ToolUseContext::agent_name`]), NOT the
        // `agent:<uuid>` id. `getAgentStatuses` matches owners against
        // name / name@team, never against an id, so writing the uuid here would
        // strand the owner. Skipped — exactly like TS when `getAgentName()`
        // returns `undefined` — when no display name is bound (the main thread /
        // leader); we must NOT write a uuid owner in that case.
        if is_agent_swarms_enabled()
            && status_input == StatusInput::State(TodoState::InProgress)
            && in_owner.is_none()
            && existing.owner.as_deref().is_none_or(str::is_empty)
        {
            if let Some(name) = ctx.agent_name.as_deref().filter(|n| !n.is_empty()) {
                new_owner = Some(name.to_string());
                updated_fields.push("owner".into());
            }
        }
        if let Some(meta) = in_metadata {
            let mut merged = existing.metadata.clone();
            for (key, value) in meta {
                if value.is_null() {
                    merged.remove(key);
                } else {
                    merged.insert(key.clone(), value.clone());
                }
            }
            new_metadata = Some(merged);
            updated_fields.push("metadata".into());
        }

        let mut status_change: Option<(TodoState, TodoState)> = None;
        if let StatusInput::State(st) = status_input {
            if st != existing.status {
                // BLOCKING TaskCompleted hooks (TaskUpdateTool.ts:230-265).
                // TS runs `executeTaskCompletedHooks` ONLY when a real
                // transition sets `status === 'completed'` (the V2 todo store's
                // sole terminal state — `failed` lives in the Product-B
                // registry, not here). Fire BEFORE applying the status: on a
                // blocking error claude-code returns `success:false` carrying
                // the reason and does NOT apply the status, so we return early
                // with that shape and leave `new_status` unset. NOT swarm-gated
                // (TS fires whenever a `TaskCompleted` hook is registered). With
                // no firer wired (`None`) this is a no-op — matching the
                // fire-and-forget registry path. Subject/description come from
                // the EXISTING task (TS `existingTask.subject` / `.description`).
                if st == TodoState::Completed {
                    if let Some(firer) = self.ctx.task_lifecycle_hooks.as_ref() {
                        if let Err(reason) = firer
                            .fire_task_completed(
                                &task_id,
                                status_wire(st),
                                &existing.subject,
                                Some(existing.description.as_str()),
                                ctx.agent_name.as_deref(),
                                ctx.team_name.as_deref(),
                            )
                            .await
                        {
                            emit_completed(
                                &bus,
                                TASK_UPDATE_COMPLETED,
                                &invocation_id,
                                duration(),
                                &[],
                            )
                            .await;
                            // claude-code wraps a blocking hook's reason with
                            // the hook's own name before it reaches the model
                            // (`Fvt("TaskCompleted", …)` over
                            // `S5t = " hook feedback:\n"`, 2.1.263
                            // `src_158021603.js` @5706), and the SAME wrapped
                            // text lands in both the `error` field and the
                            // tool_result. The sibling hooks already spell it
                            // this way (`TaskCreated` above, `Stop` and
                            // `TeammateIdle` in the orchestrator).
                            let feedback = format!("TaskCompleted hook feedback:\n{reason}");
                            return Ok(ToolCallResult {
                                data: json!({
                                    "success": false,
                                    "taskId": task_id,
                                    "updatedFields": Vec::<String>::new(),
                                    "error": feedback,
                                }),
                                model_content: Some(render_task_update_fail(
                                    &task_id,
                                    Some(&feedback),
                                )),
                                new_messages: vec![],
                                context_modifier: None,
                                is_error: false,
                                mcp_meta: None,
                            });
                        }
                    }
                }
                new_status = Some(st);
                updated_fields.push("status".into());
                status_change = Some((existing.status, st));
            }
        }

        let has_field_updates = new_subject.is_some()
            || new_description.is_some()
            || new_active_form.is_some()
            || new_owner.is_some()
            || new_status.is_some()
            || new_metadata.is_some();
        if has_field_updates {
            store
                .update(&task_id, |t| {
                    if let Some(v) = new_subject.clone() {
                        t.subject = v;
                    }
                    if let Some(v) = new_description.clone() {
                        t.description = v;
                    }
                    if let Some(v) = new_active_form.clone() {
                        t.active_form = Some(v);
                    }
                    if let Some(v) = new_owner.clone() {
                        t.owner = Some(v);
                    }
                    if let Some(v) = new_status {
                        t.status = v;
                    }
                    if let Some(v) = new_metadata.clone() {
                        t.metadata = v;
                    }
                })
                .await;
        }

        // Notify new owner via mailbox when ownership changes
        // (TaskUpdateTool.ts:277-298). Best-effort, like the TS `writeToMailbox`
        // (which swallows its own errors): a routing failure never fails the
        // TaskUpdate itself. `assignedBy` mirrors `getAgentName() || 'team-lead'`
        // — the acting teammate's DISPLAY NAME ([`ToolUseContext::agent_name`]),
        // else the literal `"team-lead"` label (NOT the `agent:<uuid>` id; the
        // recipient mailbox / `getAgentStatuses` key on names). The `from` route
        // address is that same name/label, which the router resolves to an id
        // (or accepts as the `team-lead` label).
        if is_agent_swarms_enabled() {
            if let (Some(owner), Some(router)) =
                (new_owner.clone(), self.ctx.mailbox_router.clone())
            {
                let sender_name = ctx
                    .agent_name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map_or_else(|| "team-lead".to_string(), str::to_string);
                let timestamp = iso8601_utc(std::time::SystemTime::now());
                let assignment_message = serde_json::to_string(&json!({
                    "type": "task_assignment",
                    "taskId": task_id,
                    "subject": existing.subject,
                    "description": existing.description,
                    "assignedBy": sender_name,
                    "timestamp": timestamp,
                }))
                .unwrap_or_default();
                let msg = lingxi_core::host::mailbox::MailboxMessage {
                    message_id: fresh_invocation_id(),
                    content: assignment_message,
                    timestamp: std::time::SystemTime::now(),
                    // claude-code `color: getTeammateColor()` (TaskUpdateTool.ts:294).
                    // The no-arg `getTeammateColor()` returns the SENDER session's
                    // own assigned color (or `undefined`). The Rust host does not
                    // thread a per-session teammate color into the tool context, so
                    // the faithful value here is `None` — which serializes to NO
                    // `color` key, byte-identical with claude-code's
                    // `color: undefined` for the common main-thread / `team-lead`
                    // sender that has no assigned color.
                    color: None,
                };
                // Ignore the routing result — notification is best-effort.
                let _ = router.route(&sender_name, &owner, msg).await;
            }
        }

        // addBlocks: this task blocks each listed id.
        if let Some(add_blocks) = input.get("addBlocks").and_then(Value::as_array) {
            let new_blocks: Vec<String> = add_blocks
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !existing.blocks.iter().any(|b| b == id))
                .map(str::to_string)
                .collect();
            for block_id in &new_blocks {
                store.block_task(&task_id, block_id).await;
            }
            if !new_blocks.is_empty() {
                updated_fields.push("blocks".into());
            }
        }
        // addBlockedBy: each listed id blocks this task.
        if let Some(add_blocked_by) = input.get("addBlockedBy").and_then(Value::as_array) {
            let new_blocked_by: Vec<String> = add_blocked_by
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !existing.blocked_by.iter().any(|b| b == id))
                .map(str::to_string)
                .collect();
            for blocker_id in &new_blocked_by {
                store.block_task(blocker_id, &task_id).await;
            }
            if !new_blocked_by.is_empty() {
                updated_fields.push("blockedBy".into());
            }
        }
        emit_completed(&bus, TASK_UPDATE_COMPLETED, &invocation_id, duration(), &[]).await;

        let mut content = render_task_update_success(&task_id, &updated_fields);
        // Teammate completion reminder (2.1.223 `mapToolResultToToolResultBlockParam`
        // @251903590): when a TEAMMATE closes a task to `completed` and swarms
        // are live, append the reminder after a BLANK line (`\n\n` — segment
        // table confirmed; an earlier port used a single `\n`). Gates:
        // - the COMPUTED transition's `to` (`statusChange?.to === 'completed'`);
        // - `OU()` (@242089079) = the in-process TEAMMATE context's agentId —
        //   undefined for a plain AgentTool subagent, so the port keys on
        //   `ctx.agent_name` (documented teammate-only, TS `getAgentName()`),
        //   NOT on the bare `agent_id` every subagent carries;
        // - `Jc()` swarms-enabled.
        if matches!(status_change, Some((_, TodoState::Completed)))
            && ctx.agent_name.is_some()
            && is_agent_swarms_enabled()
        {
            content.push_str("\n\nTask completed. Call TaskList now to find your next available task or see if your work unblocked others.");
        }
        // Oracle data shape (2.1.223 DCb zod): {success, taskId, updatedFields,
        // error?, statusChange?} — no `content` key; the rendered string rides
        // the model_content seam only.
        let mut data = json!({
            "success": true,
            "taskId": task_id,
            "updatedFields": updated_fields,
        });
        if let Some((from, to)) = status_change {
            data["statusChange"] = json!({ "from": status_wire(from), "to": status_wire(to) });
        }
        Ok(ToolCallResult {
            data,
            model_content: Some(content),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ==== TaskStopTool ===========================================================

// no-truncation: TaskStop emits a small fixed status string and needs no
// shared `MAX_TOOL_OUTPUT_LENGTH`/`truncate`. TaskOutput deliberately bounds
// its model-facing `<output>` via `format_task_output` (port of TS
// `formatTaskOutput`) instead of the shared 30_000-char head-truncate: a
// 32_000-char default (TASK_MAX_OUTPUT_LENGTH-overridable, clamped at 160_000)
// that keeps the TAIL behind a `[Truncated …]` header. The
// `max_result_size_chars` (100_000) value
// mirrored from claude-code's `maxResultSizeChars` is advisory metadata only —
// no production consumer enforces it, so it does NOT bound this output; the
// `format_task_output` cap is what actually bounds the TaskOutput surface.

/// `TaskStopTool` description (`TaskStopTool.ts` `async description()`).
const TASK_STOP_DESCRIPTION: &str = "Stop a running background task by ID";

/// `TaskStopTool` prompt — `DESCRIPTION` from `TaskStopTool/prompt.ts` (verbatim,
/// including the leading + trailing newlines of the TS template literal).
const TASK_STOP_PROMPT: &str = "
- Stops a running background task by its ID
- Takes a task_id parameter identifying the task to stop
- To stop an agent-team teammate, pass its agent ID (\"name@team\") or bare teammate name as task_id
- To stop a background agent spawned with a name, pass that name as task_id
- Returns a success or failure status
- Use this tool when you need to terminate a long-running task
";

static TASK_STOP_SCHEMA: Lazy<Value> = Lazy::new(|| {
    // `z.strictObject` with two OPTIONAL strings and no `required` array
    // (`TaskStopTool.ts:10-19`). `shell_id` is the deprecated KillShell field.
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "The ID of the background task to stop. Agent-team teammates and named background agents are also accepted by agent ID or name."
            },
            "shell_id": {
                "type": "string",
                "description": "Deprecated: use task_id instead"
            }
        },
        "additionalProperties": false
    })
});

/// Kills a task (surface stub).
pub struct TaskStopTool {
    ctx: BuiltinToolContext,
}

impl TaskStopTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        TASK_STOP_TOOL_NAME
    }
    /// `KillShell` is the deprecated name kept as an alias for backward
    /// compatibility (`TaskStopTool.ts:44`). `find_by_name` honours aliases.
    fn aliases(&self) -> &[&str] {
        const ALIASES: &[&str] = &["KillShell", "KillBash"];
        ALIASES
    }
    /// `searchHint: 'kill a running background task'` (`TaskStopTool.ts:41`).
    fn search_hint(&self) -> Option<&str> {
        Some("kill a running background task")
    }
    /// `userFacingName: () => 'Stop Task'` for the `external` build
    /// (`TaskStopTool.ts:46` — `''` only for `USER_TYPE === 'ant'`).
    fn user_facing_name(&self) -> Option<&str> {
        Some("Stop Task")
    }
    fn input_schema(&self) -> &Value {
        &TASK_STOP_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    /// `shouldDefer: true` (`TaskStopTool.ts:53`).
    fn should_defer(&self) -> bool {
        true
    }
    /// `isConcurrencySafe() { return true }` (`TaskStopTool.ts:54-56`).
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "TaskStop kills the named task".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        TASK_STOP_DESCRIPTION.into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        TASK_STOP_PROMPT.into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = fresh_invocation_id();
        let bus = self.ctx.bus.clone();
        // claude-code `qne(t)` — the calling agent, excluded from the
        // background-agent roster so a not-found message never suggests the
        // caller to itself.
        let caller_agent_id = ctx.agent_id.map(|id| id.to_string());

        // Resolve id: `task_id ?? shell_id`, then `if (!id)` (`TaskStopTool.ts:111-115`).
        // `??` (`Option::or`) only falls back for an absent `task_id`; a present
        // empty string survives the coalesce and is then rejected by `!id`.
        let resolved = input
            .get("task_id")
            .and_then(Value::as_str)
            .or_else(|| input.get("shell_id").and_then(Value::as_str))
            .unwrap_or("");
        if resolved.is_empty() {
            emit_failed(
                &bus,
                TASK_STOP_FAILED,
                &invocation_id,
                "missing_task_id",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(
                "Missing required parameter: task_id".into(),
            ));
        }
        let task_id = resolved.to_string();

        emit_started(&bus, TASK_STOP_STARTED, &invocation_id, &[]).await;

        let registry = match self.ctx.task_registry.clone() {
            Some(r) => r,
            None => {
                emit_failed(
                    &bus,
                    TASK_STOP_FAILED,
                    &invocation_id,
                    "registry_not_wired",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::Internal(
                    "TaskStopTool: TaskRegistryHandle not wired into BuiltinToolContext".into(),
                ));
            }
        };

        let named_agents = match self.ctx.agent_name_registry.as_ref() {
            Some(names) => names
                .list()
                .await
                .into_iter()
                .map(|(name, id)| (name, id.to_string()))
                .collect(),
            None => Vec::new(),
        };
        let record = match registry.resolve_stop_target(&task_id, &named_agents).await {
            Ok(lingxi_core::host::task_registry::TaskStopResolution::Found(record)) => record,
            Ok(lingxi_core::host::task_registry::TaskStopResolution::Ambiguous(message)) => {
                return Err(ToolError::InvalidInput(message));
            }
            Ok(lingxi_core::host::task_registry::TaskStopResolution::NotFound { suggestion }) => {
                emit_failed(
                    &bus,
                    TASK_STOP_FAILED,
                    &invocation_id,
                    "not_found",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    task_stop_not_found_message(
                        &self.ctx,
                        &registry,
                        &task_id,
                        suggestion.as_deref(),
                        caller_agent_id.as_deref(),
                    )
                    .await,
                ));
            }
            Err(error) => return Err(registry_err_to_tool_err("TaskStop", error)),
        };
        let display_id = if record.task_id == task_id {
            task_id.clone()
        } else {
            format!(
                "{} ({})",
                lingxi_core::host::display::sanitize_display(&task_id),
                record.task_id
            )
        };
        let task_id = record.task_id.clone();
        let may_stop =
            caller_may_stop(caller_agent_id.as_deref(), record.owner_agent_id.as_deref());
        let owner_refusal = || {
            ToolError::InvalidInput(format!(
                "Task {display_id} is owned by {}; agent {} cannot stop it.",
                lingxi_core::host::display::sanitize_display(
                    record.owner_agent_id.as_deref().unwrap_or(NO_OWNER_DISPLAY)
                ),
                lingxi_core::host::display::sanitize_display(
                    caller_agent_id.as_deref().unwrap_or_default()
                ),
            ))
        };
        // `td` observers check self-stop and ownership BEFORE status. An
        // observer cannot shut off its own observation loop through TaskStop.
        if record.is_observer {
            if caller_agent_id.is_some() && caller_agent_id == record.owner_agent_id {
                return Err(ToolError::InvalidInput(format!("Observer {display_id} cannot stop itself; use the task UI or a main-session TaskStop.")));
            }
            if !may_stop {
                return Err(owner_refusal());
            }
        }
        // ORDER IS THE ORACLE'S: not-running first, ownership second
        // (`src_160988549.js` @3597258 —
        // `if(I.status!=="running"&&…)throw not_running; if(!td(I)&&!sut(p,I.agentId))throw not_owner`).
        // Swapping them would tell a non-owner that a finished task is theirs to
        // stop, or refuse on ownership a task that was never running.
        let ended_with_live_loop = !record.is_parked
            && matches!(record.status.as_str(), "completed" | "failed" | "killed")
            && matches!(record.task_type.as_str(), "local_agent" | "local_workflow")
            && registry.has_live_task_loop(&task_id).await;
        if record.status != "running"
            && !record.is_observer
            && !ended_with_live_loop
            && !record.is_parked
            && !(record.task_type == "in_process_teammate"
                && registry.has_pending_teammate_departure(&task_id).await)
        {
            emit_failed(
                &bus,
                TASK_STOP_FAILED,
                &invocation_id,
                "not_running",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(format!(
                "Task {display_id} is not running (status: {})",
                record.status
            )));
        }

        // TO-04: a background task may only be stopped by the agent that owns it,
        // or by the main session. Without this, ANY subagent could stop ANY
        // other agent's background work — the `_ctx` that carries the caller's
        // identity was resolved and then ignored.
        if !record.is_observer && !may_stop {
            emit_failed(
                &bus,
                TASK_STOP_FAILED,
                &invocation_id,
                "not_owner",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(owner_refusal());
        }

        // Capture command + type from the pre-kill record. claude-code
        // (`stopTask.ts:97`) computes
        // `command = isLocalShellTask(task) ? task.command : task.description`:
        // a `local_bash` task surfaces its shell COMMAND, every other task type
        // its DESCRIPTION. The registry threads `command` (populated for
        // `local_bash` only); fall back to `description` when absent.
        let task_type = record.task_type.clone();
        let command = match record.command.clone() {
            Some(cmd) => cmd,
            None => record.description.clone(),
        };

        // claude-code's `TaskStop` tool calls the shared stop entry point with
        // `killedBy:"parent"` (2.1.220, byte-visible) — a PARENT AGENT stopping
        // one of its background children, so a killed `local_agent` notification
        // reads "was stopped by Claude" rather than the bare "was stopped".
        let stopped_process_groups = if ended_with_live_loop {
            let owners = registry.process_owners_for_task(&task_id).await;
            let mut groups = HashSet::new();
            for owner in owners {
                groups.extend(self.ctx.process.kill_owner_processes(&owner).await);
            }
            groups.len()
        } else {
            0
        };
        let already_idle_observer = record.is_observer
            && record.status != "running"
            && !record.is_parked
            && !ended_with_live_loop;
        let stopped = if already_idle_observer {
            Ok(record.clone())
        } else {
            registry.kill_with_reason(&task_id, "parent").await
        };
        if let Err(e) = stopped {
            emit_failed(
                &bus,
                TASK_STOP_FAILED,
                &invocation_id,
                "registry_error",
                started.elapsed().as_millis() as u64,
            )
            .await;
            // claude-code `rY`'s default arm: a task whose type has no stop
            // handler reports the TYPE, not a generic registry failure. Kept
            // local to TaskStop — `registry_err_to_tool_err` is shared with
            // TaskCreate / TaskOutput / TeamSpawn and with the spawn path,
            // where a `TaskStop:`-prefixed message would be wrong.
            return Err(match &e {
                lingxi_core::host::task_registry::TaskRegistryError::InvalidInput(reason)
                    if reason == "unknown task type" =>
                {
                    ToolError::InvalidInput(format!("Unsupported task type: {task_type}"))
                }
                _ => registry_err_to_tool_err("TaskStop", e),
            });
        }
        emit_completed(
            &bus,
            TASK_STOP_COMPLETED,
            &invocation_id,
            started.elapsed().as_millis() as u64,
            &[],
        )
        .await;

        // No `content` key → the orchestrator JSON-stringifies the whole data.
        let mut data = json!({
            "message": format!("Successfully stopped task: {task_id} ({command})"),
            "task_id": task_id, "task_type": task_type, "command": command,
        });
        if ended_with_live_loop {
            data["note"] = json!(format!("had already ended ({}) but its loop had not exited; re-signalled it and killed {stopped_process_groups} process group(s). The record remains listed while the loop is still live.", record.status));
        }
        Ok(ToolCallResult {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ==== TaskOutputTool =========================================================

/// `TaskOutputTool` description (`TaskOutputTool.tsx` `async description()`).
const TASK_OUTPUT_DESCRIPTION: &str = "[Deprecated] — for bash and remote_agent tasks, prefer Read on the output file path; for local_agent tasks, use the Agent tool result directly";

/// `TaskOutputTool` prompt (`TaskOutputTool.tsx` `async prompt()`, verbatim).
const TASK_OUTPUT_PROMPT: &str = "DEPRECATED: Background tasks return their output file path in the tool result, and you receive a <task-notification> with the same path when the task completes.
- For bash tasks: prefer using the Read tool on that output file path — it contains stdout/stderr.
- For local_agent tasks: use the Agent tool result directly. Do NOT Read the .output file — it is a symlink to the full subagent conversation transcript (JSONL) and will overflow your context window.
- For remote_agent tasks: prefer using the Read tool on the output file path — it contains the streamed remote session output (same as bash).

- Retrieves output from a running or completed task (background shell, agent, or remote session)
- Takes a task_id parameter identifying the task
- Returns the task output along with status information
- Use block=true (default) to wait for task completion
- Use block=false for non-blocking check of current status
- Task IDs can be found using the /tasks command
- Works with all task types: background shells, async agents, and remote sessions";

static TASK_OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    // `z.strictObject` (`TaskOutputTool.tsx:30-34`): required `task_id`, a
    // `block` bool (default true), and a `timeout` number 0..600000 (default
    // 30000 ms). The zod chain is `z.number().min(0).max(600000)` with NO
    // `.int()`, so zod-to-json-schema emits `"type":"number"` (a fractional ms
    // budget parses); `parse_timeout_ms` already reads it as `f64`. Replaces the
    // old `offset`/`limit` paging params.
    json!({
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "The task ID to get output from"
            },
            "block": {
                "type": "boolean",
                "default": true,
                "description": "Whether to wait for completion"
            },
            "timeout": {
                "type": "number",
                "minimum": 0,
                "maximum": 600_000,
                "default": 30_000,
                "description": "Max wait time in ms"
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
});

/// Default cap on the model-facing `<output>` of `TaskOutput`
/// (`outputFormatting.ts:5` `TASK_MAX_OUTPUT_DEFAULT = 32_000`).
const TASK_MAX_OUTPUT_DEFAULT: usize = 32_000;
/// Upper clamp for the `TASK_MAX_OUTPUT_LENGTH` env override
/// (`outputFormatting.ts:4` `TASK_MAX_OUTPUT_UPPER_LIMIT = 160_000`).
const TASK_MAX_OUTPUT_UPPER_LIMIT: usize = 160_000;

/// claude-code `Pir` / `hge` — the bounds `see()` clamps
/// `settings.taskOutputMaxChars` into (`src_158021603.js`: `Pir=4000,hge=128000`).
const TASK_OUTPUT_SETTING_MIN: usize = 4_000;
const TASK_OUTPUT_SETTING_MAX: usize = 128_000;

/// claude-code `bWn = u1 - c5e` = `50_000 - 32_000` — the headroom the
/// `TaskOutput` tool adds on top of the output cap to get its result budget.
const TASK_OUTPUT_RESULT_HEADROOM: usize = 18_000;

/// Boolean that also accepts the string literals `"true"`/`"false"` — a port of
/// the TS `semanticBoolean()` (`utils/semanticBoolean.ts:22-29`) preprocess
/// step: `"true"`→`true`, `"false"`→`false`; anything else passes through to the
/// inner schema (here rejected → `None`, so the caller's `default` wins).
/// Mirrors the in-repo convention in `tools/ui/src/send_message.rs`.
fn semantic_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s == "true" => Some(true),
        Value::String(s) if s == "false" => Some(false),
        _ => None,
    }
}

/// T30: parse the `TaskOutput` `timeout` (ms). claude-code's schema is
/// `z.number().min(0).max(600000).default(30000)` (`TaskOutputTool.tsx:33`).
/// JSON numbers are floats, so an integer-typed `30000`, a float-typed `30000.0`,
/// and a fractional `30000.5` all pass `z.number()`. Read as `f64` (NOT `as_u64`,
/// which rejects any non-integer JSON number and silently falls back to the 30s
/// default), default `30000` when absent/non-numeric, and clamp into the
/// `[0, 600000]` bounds the zod `.min(0).max(600000)` enforces. The result is the
/// millisecond budget as a `u64`.
fn parse_timeout_ms(input: &Value) -> u64 {
    let raw = input
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(30_000.0);
    raw.clamp(0.0, 600_000.0) as u64
}

/// Port of TS `parseInt(value, 10)`: skip leading ASCII whitespace, take an
/// optional `+`/`-` sign followed by base-10 digits, stop at the first
/// non-digit, and ignore the rest. Returns `None` for "NaN" (no leading
/// digits). Mirrors the parse used inside `validateBoundedIntEnvVar`.
fn parse_int_radix10(s: &str) -> Option<i128> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\u{000B}', '\u{000C}']);
    let mut chars = s.chars().peekable();
    let mut buf = String::new();
    if let Some(&c) = chars.peek() {
        if c == '+' || c == '-' {
            buf.push(c);
            chars.next();
        }
    }
    let mut saw_digit = false;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            buf.push(c);
            saw_digit = true;
            chars.next();
        } else {
            break;
        }
    }
    if !saw_digit {
        return None;
    }
    // Saturate huge magnitudes rather than failing: a 200-digit string is a
    // finite (very large) number in JS, which `validateBoundedIntEnvVar` then
    // caps to the upper limit. `i128::MAX` preserves the "> upper limit" path.
    Some(buf.parse::<i128>().unwrap_or(i128::MAX))
}

/// claude-code `see()` — the settings clamp: `Math.min(Math.max(e, 4000), 128000)`
/// applied to `Ge().taskOutputMaxChars`, or `None` when the setting is absent.
///
/// A value outside the range is PULLED to the nearest bound, not rejected;
/// that is what makes the setting safe to honour ahead of the env var.
fn settings_task_output_cap() -> Option<usize> {
    lingxi_core::host::session_flags::task_output_max_chars()
        .map(|v| (v as usize).clamp(TASK_OUTPUT_SETTING_MIN, TASK_OUTPUT_SETTING_MAX))
}

/// claude-code `zut()` — `see(Ge().taskOutputMaxChars) ?? 32_000`. The SOFT cap
/// the tool's result budget is derived from; unlike
/// [`max_task_output_length`] it ignores `TASK_MAX_OUTPUT_LENGTH`.
fn task_output_soft_cap() -> usize {
    settings_task_output_cap().unwrap_or(TASK_MAX_OUTPUT_DEFAULT)
}

/// Port of `getMaxTaskOutputLength()` — 2.1.263 `jqo()`:
///
/// ```js
/// function jqo(){let e=see(Ge().taskOutputMaxChars);if(e!==void 0)return e;
///   return Hte("TASK_MAX_OUTPUT_LENGTH",process.env.TASK_MAX_OUTPUT_LENGTH,c5e,tgn).effective}
/// ```
///
/// The SETTING wins outright when present (already clamped to 4_000..=128_000);
/// only when it is absent does `TASK_MAX_OUTPUT_LENGTH` apply over the 32_000
/// default, with an empty / non-positive / unparseable value falling back to
/// that default and anything above 160_000 clamped down to it. The port used to
/// read the env var alone, so a project that had set `taskOutputMaxChars` was
/// silently ignored.
fn max_task_output_length() -> usize {
    if let Some(from_settings) = settings_task_output_cap() {
        return from_settings;
    }
    let raw = match std::env::var("TASK_MAX_OUTPUT_LENGTH") {
        Ok(v) if !v.is_empty() => v,
        _ => return TASK_MAX_OUTPUT_DEFAULT,
    };
    match parse_int_radix10(&raw) {
        Some(parsed) if parsed > 0 => {
            if parsed > TASK_MAX_OUTPUT_UPPER_LIMIT as i128 {
                TASK_MAX_OUTPUT_UPPER_LIMIT
            } else {
                parsed as usize
            }
        }
        _ => TASK_MAX_OUTPUT_DEFAULT,
    }
}

/// The `[Truncated …]` header path. TS uses `getTaskOutputPath(taskId)`
/// (`diskOutput.ts:72-74`) = `<projectTempDir>/<sessionId>/tasks/<taskId>.output`.
///
/// The registry now threads the resolved ABSOLUTE spool path through
/// `TaskOutputChunk.output_path` (T11/T16), so when it is present this returns
/// the real absolute path the model can read. When the registry could not
/// resolve a path (`None`), it falls back to the deterministic filename portion
/// (`<taskId>.output`) — the truncation behaviour stays byte-faithful either way.
fn task_output_path(task_id: &str, output_path: Option<&str>) -> String {
    match output_path {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => lingxi_core::host::task_output::output_filename(task_id),
    }
}

/// Port of `formatTaskOutput` (`outputFormatting.ts:22-38`). When the output
/// length exceeds the (env-overridable) max, prepend a
/// `[Truncated. Full output: <path>]\n\n` header and keep the LAST
/// `maxLen - header.length` characters (the tail); otherwise return the output
/// unchanged.
///
/// TS measures `String.length`/`slice` in UTF-16 code units; this port measures
/// Unicode scalar values (`chars()`), which differ only for astral-plane chars.
/// For the ASCII/BMP output that task spools carry this is identical.
fn format_task_output(
    output: &str,
    task_id: &str,
    output_path: Option<&str>,
    omit_path: bool,
) -> String {
    let max_len = max_task_output_length();
    let char_count = output.chars().count();
    if char_count <= max_len {
        return output.to_string();
    }
    if omit_path {
        // claude-code `yWn`'s `omitPath` arm — there is no file to point at, so
        // the header says how much survived instead:
        //
        // ```js
        // let _=Qu(e,Math.max(0,o-hWn(o).length));
        // return{content:hWn(_.length)+_,wasTruncated:!0}
        // ```
        //
        // The budget is computed from `hWn(o)` (the CAP's digits) but the header
        // finally emitted is `hWn(_.length)` (the KEPT length) — two different
        // numbers, and the difference is what keeps the result inside the cap.
        let available = max_len.saturating_sub(truncated_tail_header(max_len).chars().count());
        let tail: String = output.chars().skip(char_count - available).collect();
        return format!("{}{tail}", truncated_tail_header(tail.chars().count()));
    }
    let header = format!(
        "[Truncated. Full output: {}]\n\n",
        task_output_path(task_id, output_path)
    );
    let available = max_len.saturating_sub(header.chars().count());
    // TS `output.slice(-availableSpace)` — keep the last `available` chars.
    let tail: String = output.chars().skip(char_count - available).collect();
    format!("{header}{tail}")
}

/// claude-code `hWn(e)` — the `omitPath` truncation header.
fn truncated_tail_header(kept_chars: usize) -> String {
    format!(
        "[Truncated to the last {kept_chars} characters; the earlier part of the report is not retrievable.]\n\n"
    )
}

/// 1:1 port of `TaskOutputTool.tsx`'s `mapToolResultToToolResultBlockParam`
/// (lines 283-308): the XML render of a `retrieval_status` + optional `task`,
/// joined by a BLANK line (`n.join("\n\n")`, 2.1.263 `src_160988549.js`
/// @3695691). Fed to the model verbatim via
/// the `content` key.
fn render_task_output(retrieval_status: &str, task: Option<&TaskOutputView>) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "<retrieval_status>{retrieval_status}</retrieval_status>"
    ));
    if let Some(t) = task {
        parts.push(format!("<task_id>{}</task_id>", t.task_id));
        parts.push(format!("<task_type>{}</task_type>", t.task_type));
        parts.push(format!("<status>{}</status>", t.status));
        // `<exit_code>` only when the task carries one (TS: defined && non-null).
        if let Some(code) = t.exit_code {
            parts.push(format!("<exit_code>{code}</exit_code>"));
        }
        // `<output>` only when the RAW trimmed output is non-blank (TS
        // `output?.trim()`), then truncate-and-format and `.trimEnd()` the
        // result (TS: `formatTaskOutput(output, task_id)` → `content.trimEnd()`).
        //
        // A `local_bash` spool goes through verbatim; EVERY other task type is
        // untrusted model-adjacent text and runs through the subagent-output
        // guard first (2.1.263 `src_160988549.js` @3695156):
        //
        // ```js
        // d = e.task.task_type==="local_bash" ? o.trimEnd()
        //   : uH(o.trimEnd(),{prependMarker:!e.task.isRawTranscript}).sanitized
        // ```
        //
        // Without it an agent whose report contained `<system-reminder>` or an
        // `antml:` tag had that text handed to the model with its control
        // syntax intact — the very hole `Task`'s own result path closes.
        if !t.output.trim().is_empty() {
            let formatted = format_task_output(
                &t.output,
                &t.task_id,
                t.output_path.as_deref(),
                t.omit_output_path,
            );
            let trimmed = formatted.trim_end();
            let body = if t.task_type == "local_bash" {
                trimmed.to_string()
            } else {
                lingxi_core::host::subagent_output_guard::sanitize_text(
                    trimmed,
                    // `prependMarker: !isRawTranscript` — a raw transcript was
                    // never a report addressed to the model, so it is
                    // neutralized silently.
                    !t.is_raw_transcript,
                )
                .sanitized
            };
            let head = t
                .harness_head
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| format!("{}\n\n", s.trim_end()))
                .unwrap_or_default();
            parts.push(format!("<output>\n{head}{body}\n</output>"));
        }
        // `<error>` AFTER `<output>` (TS `mapToolResultToToolResultBlockParam`
        // lines 299-301: `if (data.task.error) parts.push(\`<error>…</error>\`)`).
        // Only emitted for a truthy (present, non-empty) error string — the
        // agent task type's `error` field.
        if let Some(error) = t.error.as_deref().filter(|e| !e.is_empty()) {
            parts.push(format!("<error>{error}</error>"));
        }
    }
    // claude-code joins the parts with a BLANK line, not a single newline
    // (2.1.263 `src_160988549.js` @3695691: `content: r.join("\n\n")`).
    parts.join("\n\n")
}

/// claude-code `rg()` — normalise a server-supplied status line before it goes
/// in front of the model: collapse every run of whitespace (newlines included)
/// to a single space, trim, drop an empty result, and truncate a long one.
///
/// ```js
/// function v(e){if(e===void 0)return;
///   let s=e.replace(re," ").replace(/ {2,}/g," ").trim();
///   if(s==="")return;
///   return s.length>B?`${oe(s,B)}\u2026 [truncated]`:s}
/// ```
fn normalize_mcp_status_message(raw: Option<&str>) -> Option<String> {
    let collapsed = raw?.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() > MCP_STATUS_MESSAGE_MAX {
        let head: String = collapsed.chars().take(MCP_STATUS_MESSAGE_MAX).collect();
        return Some(format!("{head}\u{2026} [truncated]"));
    }
    Some(collapsed)
}

/// Cap for [`normalize_mcp_status_message`] (claude-code `B`).
const MCP_STATUS_MESSAGE_MAX: usize = 200;

/// The synthetic `mcp_task` output block — claude-code's `getTaskOutputData`
/// `mcp_task` branch (`src_160988549.js` @3690698), which returns METADATA
/// instead of the spool and sets `omitOutputPath`.
///
/// Two of the oracle's lines have no input here and are therefore omitted, not
/// invented — exactly as the oracle omits them when its own values are
/// undefined:
///
/// * `server task id: …` rides only when the SERVER's task id differs from the
///   registry id (`e.mcpTaskId!==e.id`). The port mints one id and stores no
///   separate server id, so there is nothing that could differ.
/// * `poll interval: …` rides only when `pollIntervalMs` is set; the port's
///   `McpTaskState` does not carry one.
/// * the `sep2663` protocol sentence needs a protocol field the port has not
///   modelled.
fn render_mcp_task_output(
    meta: &lingxi_core::host::task_registry::McpTaskOutputMeta,
    status: &str,
) -> String {
    let mut lines = vec![
        format!("server: {}", meta.server_name),
        format!("tool: {}", meta.tool_name),
    ];
    // Underscores render as spaces (`e.mcpStatus.replace("_"," ")`) — note the
    // oracle's non-global replace, which only touches the FIRST underscore.
    let mcp_status = meta.mcp_status.replacen('_', " ", 1);
    lines.push(if status == "killed" {
        format!("server status when stopped: {mcp_status}")
    } else {
        format!("status: {mcp_status}")
    });
    if let Some(message) = normalize_mcp_status_message(meta.status_message.as_deref()) {
        lines.push(format!("status message: {message}"));
    }
    lines.push(format!(
        "elapsed: {}",
        lingxi_core::host::shell_support::format_duration_ms(meta.elapsed_ms)
    ));
    if meta.mcp_status == "input_required" && status == "running" {
        lines.push("waiting on the user: an elicitation dialog is open".to_string());
    }
    lines.join("\n")
}

/// The `task` payload surfaced by `TaskOutputTool` — the subset of the TS
/// `TaskOutput` shape the narrow registry surface can resolve.
struct TaskOutputView {
    task_id: String,
    task_type: String,
    status: String,
    description: String,
    output: String,
    exit_code: Option<i32>,
    /// Agent-task error string (TS `TaskOutput.error`), rendered as a trailing
    /// `<error>…</error>` element after `<output>`. `None`/empty for non-agent
    /// tasks (and successful agents).
    error: Option<String>,
    /// Absolute on-disk spool path, threaded from `TaskOutputChunk.output_path`
    /// so the `[Truncated. Full output: <path>]` header shows the real path
    /// (claude-code `getTaskOutputPath(taskId)`). `None` ⟶ bare-filename fallback.
    output_path: Option<String>,
    /// claude `TaskOutput.isRawTranscript` — the body is the agent's raw
    /// transcript rather than a report it addressed to the caller. Suppresses
    /// the subagent-guard marker (`prependMarker: !isRawTranscript`);
    /// neutralisation still runs. `false` for every non-agent type.
    is_raw_transcript: bool,
    harness_head: Option<String>,
    /// claude `TaskOutput.omitOutputPath` — the body is synthetic (the
    /// `mcp_task` metadata block), so there is no spool path worth naming in a
    /// truncation header. `false` for every other type.
    omit_output_path: bool,
}

/// Byte-faithful port of `TaskOutputTool.tsx`'s `retrieval_status` decision.
///
/// TS computes (`call`, lines 219-281):
/// - non-blocking (`block=false`): terminal task → `success`, otherwise
///   `not_ready`;
/// - blocking (`block=true`): a task that is terminal (after the poll) →
///   `success`, otherwise (still running/pending) → `timeout`.
///
/// `done` is the chunk's terminal flag (`status` ∈ {completed, failed, killed});
/// the blocking call supplies the latest chunk observed by its poll loop.
fn task_output_retrieval_status(done: bool, block: bool) -> &'static str {
    if done {
        "success"
    } else if block {
        "timeout"
    } else {
        "not_ready"
    }
}

/// A task can disappear after the initial existence check, including before
/// the first output read. Both races have the same zqo timeout/null shape.
fn missing_waited_task_output() -> ToolCallResult {
    ToolCallResult {
        data: json!({
            "retrieval_status": "timeout",
            "task": null,
            "content": render_task_output("timeout", None),
        }),
        model_content: None,
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

/// Reads a task's report or spool, optionally waiting for completion.
pub struct TaskOutputTool {
    ctx: BuiltinToolContext,
}

impl TaskOutputTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        TASK_OUTPUT_TOOL_NAME
    }
    /// Backwards-compatible aliases for the renamed tools
    /// (`TaskOutputTool.tsx:150`).
    fn aliases(&self) -> &[&str] {
        const ALIASES: &[&str] = &[
            "AgentOutputTool",
            "BashOutputTool",
            "AgentOutput",
            "BashOutput",
        ];
        ALIASES
    }
    /// `searchHint: 'read output/logs from a background task'`
    /// (`TaskOutputTool.tsx:146`).
    fn search_hint(&self) -> Option<&str> {
        Some("read output/logs from a background task")
    }
    /// `userFacingName() { return 'Task Output' }` (`TaskOutputTool.tsx:151-153`).
    fn user_facing_name(&self) -> Option<&str> {
        Some("Task Output")
    }
    fn input_schema(&self) -> &Value {
        &TASK_OUTPUT_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    /// claude-code `get maxResultSizeChars(){return zut()+bWn}` — the SOFT
    /// output cap plus 18_000 of headroom, i.e. 50_000 by default and up to
    /// 146_000 when `taskOutputMaxChars` is raised. The port's flat 100_000 was
    /// neither: it over-budgeted a default session and under-budgeted a
    /// configured one.
    fn max_result_size_chars(&self) -> usize {
        task_output_soft_cap() + TASK_OUTPUT_RESULT_HEADROOM
    }

    /// claude-code `persistenceThresholdCeiling: hge+bWn` = 146_000 — the cap
    /// the persistence threshold is measured against, independent of the
    /// session's own setting.
    fn persistence_threshold_ceiling(&self) -> Option<usize> {
        Some(TASK_OUTPUT_SETTING_MAX + TASK_OUTPUT_RESULT_HEADROOM)
    }

    /// `shouldDefer: true` (`TaskOutputTool.tsx:148`).
    fn should_defer(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "TaskOutput reads spool file only".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        TASK_OUTPUT_DESCRIPTION.into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        TASK_OUTPUT_PROMPT.into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = fresh_invocation_id();
        let bus = self.ctx.bus.clone();
        // `qne(t)` — same caller exclusion as TaskStop.
        let caller_agent_id = ctx.agent_id.map(|id| id.to_string());

        // `task_id` is required by the schema; guard mirrors `validateInput`
        // (`TaskOutputTool.tsx:188-193`): `if (!task_id)` → "Task ID is required".
        let task_id = match input.get("task_id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                emit_failed(
                    &bus,
                    TASK_OUTPUT_FAILED,
                    &invocation_id,
                    "missing_task_id",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput("Task ID is required".into()));
            }
        };

        emit_started(&bus, TASK_OUTPUT_STARTED, &invocation_id, &[]).await;

        let registry = match self.ctx.task_registry.clone() {
            Some(r) => r,
            None => {
                emit_failed(
                    &bus,
                    TASK_OUTPUT_FAILED,
                    &invocation_id,
                    "registry_not_wired",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::Internal(
                    "TaskOutputTool: TaskRegistryHandle not wired into BuiltinToolContext".into(),
                ));
            }
        };

        // `block` defaults to true (TS `semanticBoolean(z.boolean().default(true))`);
        // string-coerce `"true"`/`"false"` so a quoted `block:"false"` is honoured
        // (TS `semanticBoolean` preprocess), rather than falling through to the
        // `true` default. `timeout` defaults to 30000 ms
        // (`z.number().min(0).max(600000).default(30000)`).
        let block = input.get("block").and_then(semantic_bool).unwrap_or(true);
        let timeout_ms = parse_timeout_ms(&input);

        // Existence check (`TaskOutputTool.tsx:215-218`): `if (!task) throw …`.
        // The record carries `task_type` + `description`, which the output
        // chunk does not.
        let record = match registry.get(&task_id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                emit_failed(
                    &bus,
                    TASK_OUTPUT_FAILED,
                    &invocation_id,
                    "not_found",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    task_output_not_found_message(
                        &self.ctx,
                        &registry,
                        &task_id,
                        caller_agent_id.as_deref(),
                    )
                    .await,
                ));
            }
            Err(e) => {
                emit_failed(
                    &bus,
                    TASK_OUTPUT_FAILED,
                    &invocation_id,
                    "registry_error",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(registry_err_to_tool_err("TaskOutput", e));
            }
        };

        if block {
            let _ = progress
                .send(tool_api::ToolProgress {
                    tool_use_id: ctx.tool_use_id.clone().unwrap_or_default(),
                    data: json!({
                        "type": "waiting_for_task",
                        "taskDescription": record.description,
                        "taskType": record.task_type,
                    }),
                })
                .await;
        }

        // zqo checks abort before observing even an already completed task.
        if block && timeout_ms > 0 && ctx.cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err(ToolError::Aborted);
        }

        // First (and, for `block==false`, only) read.
        let mut chunk = match registry.output(&task_id, None).await {
            Ok(c) => c,
            Err(TaskRegistryError::NotFound(_)) if block => {
                return Ok(missing_waited_task_output());
            }
            Err(e) => {
                emit_failed(
                    &bus,
                    TASK_OUTPUT_FAILED,
                    &invocation_id,
                    "registry_error",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(registry_err_to_tool_err("TaskOutput", e));
            }
        };

        // Blocking wait (`waitForTaskCompletion`, `TaskOutputTool.tsx:118-143`):
        // re-read every 100 ms until the task is terminal (`done`) or the
        // timeout elapses. Elapsed is measured on a `std::time::Instant`.
        if block && !chunk.done {
            let wait_started = Instant::now();
            while !chunk.done {
                if (wait_started.elapsed().as_millis() as u64) >= timeout_ms {
                    break;
                }
                // 2.1.263 `zqo`: abort is an error, never a timeout result.
                // Race the poll delay too, so cancellation cannot trigger one
                // more output read (or mark a newly completed task notified).
                if let Some(cancel) = ctx.cancel.as_ref() {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(ToolError::Aborted),
                        () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                    }
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                chunk = match registry.output(&task_id, None).await {
                    Ok(c) => c,
                    // The initial existence check still errors. Only a task
                    // disappearing after the blocking wait began maps to null
                    // (`zqo` → `kWn.call` in 2.1.263).
                    Err(TaskRegistryError::NotFound(_)) => {
                        emit_completed(
                            &bus,
                            TASK_OUTPUT_COMPLETED,
                            &invocation_id,
                            started.elapsed().as_millis() as u64,
                            &[],
                        )
                        .await;
                        return Ok(missing_waited_task_output());
                    }
                    Err(e) => {
                        emit_failed(
                            &bus,
                            TASK_OUTPUT_FAILED,
                            &invocation_id,
                            "registry_error",
                            started.elapsed().as_millis() as u64,
                        )
                        .await;
                        return Err(registry_err_to_tool_err("TaskOutput", e));
                    }
                };
            }
        }

        emit_completed(
            &bus,
            TASK_OUTPUT_COMPLETED,
            &invocation_id,
            started.elapsed().as_millis() as u64,
            &[],
        )
        .await;

        // Mark the task notified once its terminal output has been consumed
        // (claude-code `TaskOutputTool`: `updateTaskState(task_id, t => ({ ...t,
        // notified: true }))` in BOTH the non-blocking terminal branch
        // (`status !== 'running' && status !== 'pending'`) and the blocking
        // terminal branch (after `waitForTaskCompletion`)). `chunk.done` is true
        // for exactly those terminal statuses, so a single guarded call covers
        // both. Suppresses a later duplicate `<task-notification>` for a task the
        // model has already seen while keeping the task inspectable. Best-effort:
        // a failure here must not fail the read that already succeeded.
        if chunk.done {
            let _ = registry.mark_notified(&task_id).await;
        }

        let retrieval_status = task_output_retrieval_status(chunk.done, block);
        // For agent tasks the registry resolves a CLEAN final answer
        // (`extractTextContent(agentTask.result.content, '\n')`); prefer it over
        // the raw on-disk transcript for the model-facing `<output>` (TS
        // `getTaskOutputData` `local_agent`: `output: cleanResult || output`).
        // `result` is `None` for non-agent tasks / empty extractions, leaving
        // the raw spool content in place.
        // TO-06: an `mcp_task` returns a SYNTHETIC metadata block, never the
        // spool. Built here (the tool renders) from what the registry resolved
        // (it owns the timestamps).
        let mcp_block = chunk.mcp.as_ref().map(|meta| {
            render_mcp_task_output(
                meta,
                chunk.status.as_deref().unwrap_or(record.status.as_str()),
            )
        });
        let clean_result = chunk.result.clone().filter(|r| !r.is_empty()).or_else(|| {
            chunk
                .harness_head
                .as_ref()
                .filter(|head| !head.is_empty())
                .map(|_| "[The agent produced no report text.]".to_string())
        });
        // claude `isRawTranscript: !ue` — true exactly when the clean report was
        // empty and the body fell back to the transcript. Set ONLY in the
        // oracle's `local_agent` branch; every other type leaves it undefined,
        // so `prependMarker: !isRawTranscript` is `true` for them.
        let is_raw_transcript = record.task_type == "local_agent" && clean_result.is_none();
        let omit_output_path = mcp_block.is_some();
        let output = mcp_block
            .or(clean_result)
            .unwrap_or_else(|| chunk.content.clone());
        let view = TaskOutputView {
            task_id: chunk.task_id.clone(),
            task_type: record.task_type.clone(),
            // Status from the (latest) chunk; fall back to the record if the
            // registry could not resolve it at the chunk point.
            status: chunk
                .status
                .clone()
                .unwrap_or_else(|| record.status.clone()),
            description: record.description.clone(),
            output,
            exit_code: chunk.exit_code,
            error: chunk.error.clone(),
            // Absolute spool path threaded from the registry (T11/T16) for the
            // `[Truncated. Full output: <path>]` header.
            output_path: chunk.output_path.clone(),
            is_raw_transcript,
            harness_head: chunk.harness_head.clone(),
            omit_output_path,
        };
        let content = render_task_output(retrieval_status, Some(&view));

        // `exit_code` is optional (TS attaches it only for `local_bash`); omit
        // the key entirely when the chunk carries none. `prompt` / `result` /
        // `error` ride only for agent tasks (TS `getTaskOutputData` adds them in
        // the `local_agent` branch), so each is emitted only when present.
        let mut task_obj = Map::new();
        task_obj.insert("task_id".into(), json!(view.task_id));
        task_obj.insert("task_type".into(), json!(view.task_type));
        task_obj.insert("status".into(), json!(view.status));
        task_obj.insert("description".into(), json!(view.description));
        task_obj.insert("output".into(), json!(view.output));
        if let Some(head) = &view.harness_head {
            task_obj.insert("harnessHead".into(), json!(head));
        }
        if let Some(code) = view.exit_code {
            task_obj.insert("exit_code".into(), json!(code));
        }
        if let Some(prompt) = &chunk.prompt {
            task_obj.insert("prompt".into(), json!(prompt));
        }
        if view.task_type == "local_agent" {
            task_obj.insert("result".into(), json!(view.output));
            task_obj.insert("isRawTranscript".into(), json!(view.is_raw_transcript));
        }
        if let Some(error) = view.error.as_deref().filter(|e| !e.is_empty()) {
            task_obj.insert("error".into(), json!(error));
        }

        // Nested `{ retrieval_status, task: { … } }` plus the `content` render so
        // the model sees it (`TaskOutputTool.tsx` data + `…BlockParam`).
        Ok(ToolCallResult {
            data: json!({
                "retrieval_status": retrieval_status,
                "task": Value::Object(task_obj),
                "content": content,
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
#[path = "task_test.rs"]
mod task_test;
