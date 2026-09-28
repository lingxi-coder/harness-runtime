//! Pure `From<engine type>` lowering fns — the parity surface (plan F1-11).
//!
//! This module is the single place every engine runtime type is mechanically
//! lowered into a `client::protocol` DTO. The fns are PURE (no `async`, no I/O,
//! no engine calls) so both transports (bridge-server WS, mobile `UniFFI`) and the
//! parity tests (F1-15) share one mapping. Keeping the mapping here — not inline
//! in the `OutputStream`/`PermissionGate` impls — is why structural parity is
//! testable without driving a live turn.
//!
//! ## Lowering rules (plan F1-11)
//!
//! - `serde_json::Value` → JSON **String** (`input_json` / `result_json`); this
//!   conversion keeps JSON values out of the native-compatible protocol DTOs.
//! - `SystemTime` → RFC 3339 `String`, byte-identical to
//!   `session::jsonl::loader::format_rfc3339_seconds` so the session-picker
//!   timestamp renders the same on every surface (parity, plan line 152).
//! - `Duration` → `u64` whole seconds; `usize` → `u32` (saturating — a row count
//!   never realistically exceeds `u32::MAX`, and saturating is lossless in
//!   practice while avoiding a panic).
//! - `McpStatus::Error(String)` (tuple) → [`McpStatusDto::Error { reason }`]
//!   (struct, for `UniFFI` flatness — decision §0.4 / plan line 154).
//! - `PromptDefault` → `bool` (`AllowByDefault` ⇒ `true`) — the collapsed
//!   `default_allow` carried by `PermissionKindDto::ToolUseConfirm`.
//! - `CostSnapshot` → [`CostDto`] (`Duration` → secs; `formatted` rendered as
//!   `"${:.4}"`, matching `tui::events::orchestrator_bridge` line 152 parity).
//! - `SessionMetadata` → [`SessionRowDto`] (`.path` mapped DIRECTLY — it is a
//!   real field, NOT synthesized, plan line 152).
//! - `McpServerInfo` / `HookInfo` / `AgentInfo` / `StatusSnapshot` /
//!   `DoctorReport` / `TaskRecord` / `TaskOutputChunk` → their DTOs.
//!
//! `GroupedToolUse` / `CollapsedReadSearch` folding is deliberately NOT here —
//! that stays CLIENT-SIDE; the adapter emits the raw `ToolUseStarted` /
//! `ToolUseResult` per the events catalog (plan F1-11).

/// Restore the exact API counters after SessionResumed resets client state.
#[must_use]
pub fn lower_current_usage(
    usage: platform_api::CurrentUsageSnapshot,
) -> crate::protocol::events::ClientEvent {
    crate::protocol::events::ClientEvent::UsageUpdate {
        is_snapshot: Some(true),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_input_tokens,
        cache_creation_tokens: usage.cache_creation_input_tokens,
    }
}

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::protocol::events::CostDto;
use crate::protocol::listings::{
    AgentDto, CheckStatusDto, CoordinatorWorkerDto, DoctorCheckDto, DoctorReportDto,
    DoctorSummaryDto, HookDto, McpServerDto, McpStatusDto, SessionModeDto, SessionRowDto, SkillDto,
    StatusSnapshotDto, TaskRowDto, TaskStatusDto,
};

use permission::PromptDefault;
use platform_api::orchestrator::{
    AgentInfo, CheckStatus, CostSnapshot, DoctorCheck, DoctorReport, DoctorSummary, HookInfo,
    McpServerInfo, McpStatus, SkillInfo, StatusSnapshot,
};
use platform_api::task_registry::{TaskOutputChunk, TaskRecord};
use platform_api::team_registry::WorkerInfo;
use session::jsonl::loader::SessionMetadata;

pub use crate::adapter::controls::lower_reasoning_control_spec;

// ── Primitive lowering rules ───────────────────────────────────────────────

/// Lower a tool input/result `serde_json::Value` to its JSON **String** wire
/// form (`input_json` / `result_json`).
///
/// This is the boundary decision §0.4 names: `serde_json::Value` is not
/// UniFFI-representable, so it never enters `client::protocol`. The string keeps
/// `preserve_order` key ordering (the workspace `serde_json` feature) so the
/// lowered bytes match the inbound tool-call bytes. `to_string` on a `Value`
/// cannot fail, so this is infallible.
#[must_use]
pub fn value_to_json_string(value: &serde_json::Value) -> String {
    value.to_string()
}

/// Lower a `SystemTime` to a seconds-resolution RFC 3339 `String`.
///
/// Byte-identical to `session::jsonl::loader::format_rfc3339_seconds` (the
/// session-picker / resume-screen helper) so the lowered timestamp renders the
/// same on every surface. Pre-1970 inputs (never produced by file mtime on the
/// platforms we target) fall back to the Unix-epoch literal.
#[must_use]
pub fn system_time_to_rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    #[allow(clippy::cast_possible_wrap)]
    let secs_i64 = secs as i64;
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs_i64, 0).map_or_else(
        || "1970-01-01T00:00:00Z".to_string(),
        |dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// Lower a `Duration` to whole seconds (`session_duration` → `*_secs`).
#[must_use]
pub fn duration_to_secs(d: Duration) -> u64 {
    d.as_secs()
}

/// Lower a `usize` count to a `u32` wire field, saturating at `u32::MAX`.
///
/// A message / row count never realistically exceeds `u32::MAX`; saturating is
/// lossless in practice and avoids a `try_into` panic on a pathological input.
#[must_use]
pub fn usize_to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Lower a `PromptDefault` to the collapsed `default_allow: bool` carried by
/// `PermissionKindDto::ToolUseConfirm`. `AllowByDefault` ⇒ `true`.
#[must_use]
pub fn prompt_default_to_allow(d: PromptDefault) -> bool {
    matches!(d, PromptDefault::AllowByDefault)
}

// ── Enum lowering rules ────────────────────────────────────────────────────

/// Lower the engine's `McpStatus` (with a tuple `Error(String)`) to the DTO's
/// STRUCT-variant `McpStatusDto::Error { reason }` (`UniFFI` flatness, §0.4).
#[must_use]
pub fn lower_mcp_status(status: &McpStatus) -> McpStatusDto {
    match status {
        McpStatus::Connected => McpStatusDto::Connected,
        McpStatus::Disconnected => McpStatusDto::Disconnected,
        McpStatus::Error(reason) => McpStatusDto::Error {
            reason: reason.clone(),
        },
    }
}

/// Lower a `/doctor` `CheckStatus` to its DTO.
#[must_use]
pub fn lower_check_status(status: &CheckStatus) -> CheckStatusDto {
    match status {
        CheckStatus::Pass => CheckStatusDto::Pass,
        CheckStatus::Warn => CheckStatusDto::Warn,
        CheckStatus::Fail => CheckStatusDto::Fail,
    }
}

/// Lower a `TaskRecord`'s `status` wire `String` to a [`TaskStatusDto`].
///
/// The engine's terminal `"killed"` wire status maps to
/// [`TaskStatusDto::Cancelled`] (the DTO's user-stop variant — the names were
/// reconciled in F1-05). An unrecognized status falls back to
/// [`TaskStatusDto::Pending`] (the safest non-terminal default — a future
/// status would be additive on the `#[non_exhaustive]` enum).
#[must_use]
pub fn lower_task_status(wire: &str) -> TaskStatusDto {
    match wire {
        "running" => TaskStatusDto::Running,
        "paused" => TaskStatusDto::Paused,
        "completed" => TaskStatusDto::Completed,
        "failed" => TaskStatusDto::Failed,
        "killed" => TaskStatusDto::Cancelled,
        // "pending" and any unknown/future status fall back to Pending.
        _ => TaskStatusDto::Pending,
    }
}

// ── Struct lowering rules ──────────────────────────────────────────────────

/// Lower a `CostSnapshot` to a [`CostDto`] (the display-relevant fields).
///
/// `session_duration` (`Duration`) → `session_duration_secs` (`u64`);
/// `formatted` is rendered `"${:.4}"`, matching the TUI bridge (parity).
#[must_use]
pub fn lower_cost_snapshot(cost: &CostSnapshot) -> CostDto {
    CostDto {
        total_usd: cost.total_usd,
        input_tokens: cost.input_tokens,
        output_tokens: cost.output_tokens,
        api_calls: cost.api_calls,
        session_duration_secs: duration_to_secs(cost.session_duration),
        formatted: format!("${:.4}", cost.total_usd),
    }
}

/// Lower a `SessionMetadata` to a [`SessionRowDto`].
///
/// `uuid` → its string form; `modified` (`SystemTime`) → RFC 3339;
/// `message_count` (`usize`) → `u32`; `path` (`PathBuf`) is mapped DIRECTLY via
/// a lossy display string (plan line 152 — not synthesized).
#[must_use]
pub fn lower_session_metadata(meta: &SessionMetadata) -> SessionRowDto {
    SessionRowDto {
        uuid: meta.uuid.to_string(),
        title: meta.title.clone(),
        modified_rfc3339: system_time_to_rfc3339(meta.modified),
        message_count: usize_to_u32(meta.message_count),
        mode: match meta.mode {
            session::jsonl::SessionMode::Chat => SessionModeDto::Chat,
            session::jsonl::SessionMode::Code => SessionModeDto::Code,
        },
        path: meta.path.to_string_lossy().into_owned(),
    }
}

/// Lower a `McpServerInfo` to a [`McpServerDto`].
#[must_use]
pub fn lower_mcp_server_info(info: &McpServerInfo) -> McpServerDto {
    McpServerDto {
        name: info.name.clone(),
        status: lower_mcp_status(&info.status),
        transport: info.transport.clone(),
    }
}

/// Lower a `SkillInfo` to a [`SkillDto`]. `source_dir` (a `PathBuf`) is
/// rendered as a display string, matching `lower_session_metadata`'s
/// `path` handling — not synthesized, the engine field mapped directly.
#[must_use]
pub fn lower_skill_info(info: &SkillInfo) -> SkillDto {
    SkillDto {
        name: info.name.clone(),
        source_dir: info.source_dir.to_string_lossy().into_owned(),
    }
}

/// Lower a `HookInfo` to a [`HookDto`].
#[must_use]
pub fn lower_hook_info(info: &HookInfo) -> HookDto {
    HookDto {
        name: info.name.clone(),
        event: info.event.clone(),
        matcher: info.matcher.clone(),
        timeout_ms: info.timeout_ms,
        hook_type: Some(info.hook_type.clone()),
        source: Some(info.source.clone()),
        content: Some(info.content.clone()),
        status_message: info.status_message.clone(),
        blocking: Some(info.blocking),
        is_async: Some(!info.blocking),
        priority: Some(info.priority),
        async_rewake: Some(info.async_rewake),
        async_timeout_ms: info.async_timeout_ms,
        if_condition: info.if_condition.clone(),
    }
}

/// Lower an `AgentInfo` to an [`AgentDto`].
#[must_use]
pub fn lower_agent_info(info: &AgentInfo) -> AgentDto {
    AgentDto {
        name: info.name.clone(),
        description: info.description.clone(),
        tools_allowed: info.tools_allowed.clone(),
    }
}

/// Lower a `StatusSnapshot` to a [`StatusSnapshotDto`].
///
/// The traits-shape fields map 1:1; the appended optional `status_line` is left
/// `None` here (it is a status-line addition the engine struct does not carry —
/// plan line 155 — so a caller with a pre-rendered line sets it after lowering).
///
/// The engine's `active_workers` (`u32`) is carried through as
/// `Some(active_workers)` (T21) so `/status` echoes the live coordinator-team
/// worker count. It is `0` (still emitted as `Some(0)`) for a non-coordinator
/// session — the wire skip happens only when the optional is `None`, which this
/// lowering never produces, matching the always-present engine field.
#[must_use]
pub fn lower_status_snapshot(s: &StatusSnapshot) -> StatusSnapshotDto {
    StatusSnapshotDto {
        session_id: s.session_id.clone(),
        model: s.model.clone(),
        n_messages: s.n_messages,
        total_cost_usd: s.total_cost_usd,
        input_tokens: s.input_tokens,
        output_tokens: s.output_tokens,
        n_mcp_connected: s.n_mcp_connected,
        n_mcp_total: s.n_mcp_total,
        n_hooks: s.n_hooks,
        n_agents: s.n_agents,
        started_at: s.started_at.clone(),
        cwd: s.cwd.to_string_lossy().into_owned(),
        status_line: None,
        active_workers: Some(s.active_workers),
    }
}

/// Lower a `/doctor` `DoctorCheck` to a [`DoctorCheckDto`].
#[must_use]
pub fn lower_doctor_check(check: &DoctorCheck) -> DoctorCheckDto {
    DoctorCheckDto {
        name: check.name.clone(),
        status: lower_check_status(&check.status),
        detail: check.detail.clone(),
    }
}

/// Lower a `DoctorSummary` to a [`DoctorSummaryDto`].
#[must_use]
pub fn lower_doctor_summary(summary: &DoctorSummary) -> DoctorSummaryDto {
    DoctorSummaryDto {
        passed: summary.passed,
        warnings: summary.warnings,
        failed: summary.failed,
    }
}

/// Lower a `DoctorReport` to a [`DoctorReportDto`] (checks + summary).
#[must_use]
pub fn lower_doctor_report(report: &DoctorReport) -> DoctorReportDto {
    DoctorReportDto {
        checks: report.checks.iter().map(lower_doctor_check).collect(),
        summary: lower_doctor_summary(&report.summary),
    }
}

/// Lower a `TaskRecord` to a [`TaskRowDto`] (the `status` wire `String` is
/// lowered to a [`TaskStatusDto`] enum).
#[must_use]
pub fn lower_task_record(rec: &TaskRecord) -> TaskRowDto {
    TaskRowDto {
        // For shell tasks owner_agent_id identifies the creator, not a runner.
        agent_id: if rec.task_type == "local_agent" {
            rec.owner_agent_id.clone()
        } else {
            None
        },
        unread: rec.status == "completed" && !rec.notified,
        model: rec.model.clone(),
        effort: rec.effort.clone(),
        kind: rec.kind.clone(),
        awaiting_plan_approval: rec.awaiting_plan_approval,
        task_id: rec.task_id.clone(),
        task_type: rec.task_type.clone(),
        status: lower_task_status(&rec.status),
        description: rec.description.clone(),
        // The reduced task record intentionally keeps no script/checkpoint
        // paths. The concrete mobile command performs the stronger metadata
        // validation before launching; this flag is only an affordance hint.
        can_resume: rec.task_type == "local_workflow" && rec.status == "paused",
        started_at_ms: rec.started_at_ms,
        // Terminal failure reason, when the handler reported one. Only a
        // `failed` row can carry one, so a non-failed record never leaks a
        // stale reason onto the panel.
        error: if rec.status == "failed" {
            rec.error.clone()
        } else {
            None
        },
        stage: rec.stage.clone(),
    }
}

/// Lower a `platform_api::team_registry::WorkerInfo` (the POD projection of the
/// coordinator's `WorkerAgent`) to a [`CoordinatorWorkerDto`] (T18).
///
/// The mapping is 1:1 — `WorkerInfo` is already the simplified roster shape that
/// the DTO and the TUI `WorkerRow` share (`agent_id` / `name` / `agent_type` /
/// `status`, with `status` a plain label `String`). No `WorkerStatus` enum is
/// touched here; the simplification happens in the `coordinator`-side
/// `TeamRegistryHandle` impl (T17).
#[must_use]
pub fn lower_worker_agent(info: &WorkerInfo) -> CoordinatorWorkerDto {
    CoordinatorWorkerDto {
        agent_id: info.agent_id.clone(),
        name: info.name.clone(),
        agent_type: info.agent_type.clone(),
        status: if info.awaiting_plan_approval {
            "awaiting approval".into()
        } else {
            info.status.clone()
        },
    }
}

/// One lowered task-output chunk: the `(task_id, content, total_lines,
/// truncated)` tuple the `ClientEvent::TaskOutputChunk` variant carries.
///
/// `TaskOutputChunk` has no standalone DTO struct (it is carried inline by the
/// event), so this returns the field tuple the event is built from.
#[must_use]
pub fn lower_task_output_chunk(chunk: &TaskOutputChunk) -> (String, String, u64, bool) {
    (
        chunk.task_id.clone(),
        chunk.content.clone(),
        chunk.total_lines,
        chunk.truncated,
    )
}

mod messages;
mod models;
pub use messages::{
    lower_conversation_message, lower_conversation_message_with, lower_transcript,
    lower_transcript_with_tool_results,
};
pub use models::{lower_model_details, lower_provider_model_catalog_entry};

#[cfg(test)]
mod tests;
