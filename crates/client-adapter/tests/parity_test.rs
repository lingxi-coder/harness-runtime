//! Listing / screen `From` parity tests (plan F1-15).
//!
//! ## What "parity" means here
//!
//! For every engine info struct a TUI renderer consumes, this suite feeds the
//! IDENTICAL engine input to the adapter's F1-11 lowering fn and asserts the
//! resulting DTO carries the same structural fields the TUI extracts. Parity is
//! STRUCTURAL — the DTO field set / values are the lowered engine fields, never
//! re-derived through a different code path. So if the engine struct grows a
//! field the renderer reads, this suite is the place the DTO is proven to carry
//! it too.
//!
//! ## Why the fixtures are reconstructed inline (no `tui` dependency)
//!
//! `client-adapter` must NOT depend on `tui` (plan F1-10 / `Cargo.toml` note):
//! `BridgeOutputStream`/`TuiPermissionGate` are reference templates to COPY, and
//! a `tui` edge — even a dev-only one — would couple the engine tier to the UI.
//! The TUI render fixtures are themselves just plain engine-type constructions
//! (`SessionMetadata`, `StatusSnapshot`, `TaskRecord`, …), so each fixture below
//! is reconstructed byte-for-byte from its TUI counterpart with the TUI source
//! line cited as the parity anchor. Reconstructing the value (rather than
//! importing the `tui` test module) keeps the adapter `tui`-free while still
//! driving the SAME input the TUI renderer consumes.
//!
//! Coverage: one parity test per F1-11 listing lowering fn that has a TUI
//! renderer counterpart — `SessionMetadata` (resume screen), `StatusSnapshot`
//! (status tab), `AgentInfo` (agents screen), `TaskRecord` / `TaskOutputChunk`
//! (multi-agent task list), plus the listing types whose engine source is live
//! but stubbed in the TUI (`McpServerInfo`, `HookInfo`, `DoctorReport`) driven
//! through deterministic fixtures (plan F1-15: "real where live, fixtures where
//! stubbed").

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

use client_adapter::lowering::{
    lower_agent_info, lower_doctor_report, lower_hook_info, lower_mcp_server_info,
    lower_session_metadata, lower_status_snapshot, lower_task_output_chunk, lower_task_record,
};
use client_protocol::listings::{CheckStatusDto, McpStatusDto, TaskStatusDto};

use platform_api::orchestrator::{
    AgentInfo, CheckStatus, DoctorCheck, DoctorReport, DoctorSummary, HookInfo, McpServerInfo,
    McpStatus, StatusSnapshot,
};
use platform_api::task_registry::{TaskOutputChunk, TaskRecord};
use session::jsonl::loader::SessionMetadata;

// ── Sessions (resume screen) ───────────────────────────────────────────────

/// Parity anchor: `tui/src/screens/resume.rs` `tests::meta()` builds exactly
/// this `SessionMetadata`, and `ResumeRow::from_meta` renders it. The resume row
/// asserts `modified_label == "2025-05-24T19:03:12Z"` and `count_label` derived
/// from `message_count`; the DTO must carry the SAME lowered timestamp and count
/// (plus `path`, which the row drops but the DTO maps DIRECTLY — plan line 152).
#[test]
fn session_metadata_parity() {
    // Identical to `tui::screens::resume::tests::meta("hello", 1_748_113_392, 1)`.
    let meta = SessionMetadata {
        uuid: uuid::Uuid::nil(),
        mode: session::jsonl::SessionMode::Code,
        title: "hello".to_string(),
        modified: UNIX_EPOCH + Duration::from_secs(1_748_113_392),
        // SESSION.6: created (file birthtime) tie-break key; the lowered DTO /
        // resume row asserts modified/count/path, not created, so this value is
        // immaterial to the assertions below.
        created: UNIX_EPOCH + Duration::from_secs(1_748_113_392),
        message_count: 1,
        path: PathBuf::from("/tmp/x.jsonl"),
        pr_number: None,
        // Search-only field; `lower_session_metadata` does not map it, so the
        // DTO assertions below are unaffected by its value.
        custom_or_ai_title: Some("hello".to_string()),
        resume_model: None,
        resume_model_profile: None,
    };

    let dto = lower_session_metadata(&meta);

    // The TUI's `ResumeRow.modified_label` is `format_rfc3339_seconds(modified)`;
    // the DTO's `modified_rfc3339` is the same byte-for-byte lowering.
    assert_eq!(dto.modified_rfc3339, "2025-05-24T19:03:12Z");
    assert_eq!(dto.title, "hello");
    assert_eq!(dto.uuid, uuid::Uuid::nil().to_string());
    // The row derives `(1 message)` from `message_count == 1`; the DTO carries
    // the raw count the client re-pluralizes — structurally the same source.
    assert_eq!(dto.message_count, 1);
    // `.path` is mapped DIRECTLY (the row drops it; the DTO keeps it so a client
    // can request a re-load — plan line 152, NOT synthesized).
    assert_eq!(dto.path, "/tmp/x.jsonl");
}

// ── Status (settings status tab) ───────────────────────────────────────────

/// Parity anchor: `tui/src/screens/settings/status.rs` `tests::fixture()` builds
/// exactly this `StatusSnapshot`, and `render_status_to_string` renders rows like
/// `"Model: claude-opus-4-8"`, `"MCP servers: 1 connected / 3 configured"`,
/// `"Session ID: sess-abc123"`. The DTO must carry the SAME field values the
/// renderer reads (every traits-shape field 1:1; `status_line` appended `None`).
#[test]
#[allow(clippy::float_cmp)] // `total_cost_usd` is copied verbatim (no arithmetic).
fn status_snapshot_parity() {
    // Identical to `tui::screens::settings::status::tests::fixture().status`.
    let snap = StatusSnapshot {
        session_id: "sess-abc123".to_string(),
        model: "claude-opus-4-8".to_string(),
        model_profile: None,
        n_messages: 12,
        total_cost_usd: 0.0421,
        input_tokens: 3400,
        output_tokens: 1200,
        n_mcp_connected: 1,
        n_mcp_total: 3,
        n_hooks: 2,
        n_agents: 4,
        started_at: "2026-05-29T10:00:00Z".to_string(),
        cwd: PathBuf::from("/home/u/proj"),
        active_workers: 0,
        setting_sources: Vec::new(),
    };

    let dto = lower_status_snapshot(&snap);

    // Each value the status renderer reads is carried 1:1 by the DTO.
    assert_eq!(dto.session_id, "sess-abc123"); // "Session ID: sess-abc123"
    assert_eq!(dto.model, "claude-opus-4-8"); // "Model: claude-opus-4-8"
    assert_eq!(dto.n_messages, 12);
    assert_eq!(dto.total_cost_usd, 0.0421);
    assert_eq!(dto.input_tokens, 3400);
    assert_eq!(dto.output_tokens, 1200);
    assert_eq!(dto.n_mcp_connected, 1); // "MCP servers: 1 connected / 3 configured"
    assert_eq!(dto.n_mcp_total, 3);
    assert_eq!(dto.n_hooks, 2);
    assert_eq!(dto.n_agents, 4);
    assert_eq!(dto.started_at, "2026-05-29T10:00:00Z");
    assert_eq!(dto.cwd, "/home/u/proj");
    // The appended status-line field is `None` on lowering (the engine struct
    // does not carry it — plan line 155); a caller with a pre-rendered line sets
    // it after lowering.
    assert_eq!(dto.status_line, None);
}

// ── Agents (agents screen) ─────────────────────────────────────────────────

/// Parity anchor: `tui/src/screens/agents.rs` renders an `AgentRow` whose
/// `name`/`description`/`tools` come straight from the wire `AgentInfo` (doc
/// comment, agents.rs:7-8). `render_agent_detail` prints `Tools: Read, Grep`.
/// The DTO must carry the SAME three live-from-wire fields (the richer
/// `AgentRow` detail fields are fixture/`None` and have no engine source, so
/// they are intentionally NOT on the DTO).
#[test]
fn agent_info_parity() {
    // The three live-from-wire fields the `AgentRow` reads from `AgentInfo`.
    let info = AgentInfo {
        name: "explorer".to_string(),
        description: "find things".to_string(),
        tools_allowed: vec!["Read".to_string(), "Grep".to_string()],
        wildcard_tools: false,
        ..AgentInfo::default()
    };

    let dto = lower_agent_info(&info);

    assert_eq!(dto.name, "explorer");
    assert_eq!(dto.description, "find things");
    // The TUI detail renders `Tools: Read, Grep` from this same Vec.
    assert_eq!(
        dto.tools_allowed,
        vec!["Read".to_string(), "Grep".to_string()]
    );
}

// ── Tasks (multi-agent task list) ──────────────────────────────────────────

/// Parity anchor: `tui/src/multiagent/poller.rs`
/// `tests::poll_maps_registry_records_to_task_rows` feeds exactly these two
/// `TaskRecord`s and asserts the poller maps them one-to-one onto a
/// `multiagent::state::TaskRow` (which mirrors `TaskRecord` field-for-field,
/// state.rs:7-20). The adapter lowers the SAME records onto `TaskRowDto`; the
/// one transform is the `status` wire `String` → `TaskStatusDto` enum.
#[test]
fn task_record_parity() {
    // Identical to the two records in the poller test.
    let running = TaskRecord {
        task_id: "b00000001".to_string(),
        task_type: "local_bash".to_string(),
        status: "running".to_string(),
        description: "cargo build".to_string(),
        command: None,
        ..Default::default()
    };
    let completed = TaskRecord {
        task_id: "a00000002".to_string(),
        task_type: "local_agent".to_string(),
        status: "completed".to_string(),
        description: "explore".to_string(),
        command: None,
        ..Default::default()
    };

    let running_dto = lower_task_record(&running);
    let completed_dto = lower_task_record(&completed);

    // Field-for-field parity with the `TaskRow` the poller produces …
    assert_eq!(running_dto.task_id, "b00000001");
    assert_eq!(running_dto.task_type, "local_bash");
    assert_eq!(running_dto.description, "cargo build");
    // … with the one transform: the wire `status` String → the DTO enum.
    assert_eq!(running_dto.status, TaskStatusDto::Running);

    assert_eq!(completed_dto.task_id, "a00000002");
    assert_eq!(completed_dto.task_type, "local_agent");
    assert_eq!(completed_dto.description, "explore");
    assert_eq!(completed_dto.status, TaskStatusDto::Completed);
}

#[test]
fn task_agent_identity_is_the_runner_and_never_a_shell_creator() {
    for task_type in [
        "local_agent",
        "local_bash",
        "remote_agent",
        "in_process_teammate",
    ] {
        let record = TaskRecord {
            task_type: task_type.into(),
            owner_agent_id: Some("agent:runner-or-creator".into()),
            ..Default::default()
        };
        assert_eq!(
            lower_task_record(&record).agent_id,
            (task_type == "local_agent").then(|| "agent:runner-or-creator".into()),
            "{task_type} must only link an actual runner identity",
        );
    }
    assert_eq!(
        lower_task_record(&TaskRecord {
            task_type: "local_agent".into(),
            ..Default::default()
        })
        .agent_id,
        None,
    );
}

/// Parity anchor: `tui/src/components/tasks/output_tail.rs` `tests` drive a
/// `TaskOutputChunk` (`content: "a\nb\nc\nd\ne"`, `total_lines: 5`) through the
/// output-tail state. `TaskOutputChunk` has no standalone DTO struct — it is
/// carried inline by `ClientEvent::TaskOutputChunk`, so the lowering returns the
/// field tuple the event is built from; the tuple must mirror the chunk the TUI
/// tail consumes.
#[test]
fn task_output_chunk_parity() {
    // Identical shape to the `output_tail.rs` fixture chunk.
    let chunk = TaskOutputChunk {
        task_id: "b00000001".to_string(),
        content: "a\nb\nc\nd\ne".to_string(),
        total_lines: 5,
        truncated: true,
        ..Default::default()
    };

    let (task_id, content, total_lines, truncated) = lower_task_output_chunk(&chunk);

    assert_eq!(task_id, "b00000001");
    assert_eq!(content, "a\nb\nc\nd\ne");
    assert_eq!(total_lines, 5);
    assert!(truncated);
}

// ── MCP (live engine source, stubbed in the TUI) ───────────────────────────

/// `McpServerInfo` is a live `OrchestratorHandle::list_mcp_servers` source; the
/// TUI screen is stubbed, so this drives a deterministic fixture (plan F1-15:
/// "real where live, fixtures where stubbed"). The parity point is the
/// `McpStatus::Error(String)` TUPLE → `McpStatusDto::Error { reason }` STRUCT
/// lowering (`UniFFI` flatness, decision §0.4) — the field the renderer would
/// read as the failure reason survives the shape change.
#[test]
fn mcp_server_info_parity() {
    let connected = McpServerInfo {
        name: "fs".to_string(),
        status: McpStatus::Connected,
        transport: "stdio".to_string(),
    };
    let errored = McpServerInfo {
        name: "remote".to_string(),
        status: McpStatus::Error("handshake failed".to_string()),
        transport: "http".to_string(),
    };

    let connected_dto = lower_mcp_server_info(&connected);
    assert_eq!(connected_dto.name, "fs");
    assert_eq!(connected_dto.transport, "stdio");
    assert_eq!(connected_dto.status, McpStatusDto::Connected);

    let errored_dto = lower_mcp_server_info(&errored);
    assert_eq!(errored_dto.name, "remote");
    assert_eq!(errored_dto.transport, "http");
    // The engine's tuple `Error(String)` → the DTO's struct variant; the reason
    // the renderer would surface is preserved across the flatten.
    assert_eq!(
        errored_dto.status,
        McpStatusDto::Error {
            reason: "handshake failed".to_string()
        }
    );
}

// ── Hooks (live engine source, stubbed in the TUI) ─────────────────────────

/// `HookInfo` is a live `OrchestratorHandle::list_hooks` source; deterministic
/// fixture. The parity point is the optional `matcher` round-tripping (present
/// and absent) — the field a hooks renderer shows or hides.
#[test]
fn hook_info_parity() {
    let with_matcher = HookInfo {
        name: "guard".to_string(),
        event: "PreToolUse".to_string(),
        matcher: Some("Bash.*".to_string()),
        timeout_ms: 5_000,
        ..HookInfo::default()
    };
    let without_matcher = HookInfo {
        name: "stop-logger".to_string(),
        event: "Stop".to_string(),
        matcher: None,
        timeout_ms: 60_000,
        ..HookInfo::default()
    };

    let with_dto = lower_hook_info(&with_matcher);
    assert_eq!(with_dto.name, "guard");
    assert_eq!(with_dto.event, "PreToolUse");
    assert_eq!(with_dto.matcher.as_deref(), Some("Bash.*"));
    assert_eq!(with_dto.timeout_ms, 5_000);

    let without_dto = lower_hook_info(&without_matcher);
    assert_eq!(without_dto.name, "stop-logger");
    assert_eq!(without_dto.event, "Stop");
    assert_eq!(without_dto.matcher, None);
    assert_eq!(without_dto.timeout_ms, 60_000);
}

// ── Doctor (live engine source, stubbed in the TUI) ────────────────────────

/// `DoctorReport` is a live `OrchestratorHandle::run_doctor_checks` source;
/// deterministic fixture. The parity point is the nested
/// `CheckStatus` → `CheckStatusDto` lowering, the optional `detail` (shown on a
/// second indented line if `Some`), and the summary tallies — every field a
/// `/doctor` renderer prints.
#[test]
fn doctor_report_parity() {
    let report = DoctorReport {
        checks: vec![
            DoctorCheck {
                name: "config-dir".to_string(),
                status: CheckStatus::Pass,
                detail: None,
            },
            DoctorCheck {
                name: "api-key".to_string(),
                status: CheckStatus::Warn,
                detail: Some("expires soon".to_string()),
            },
            DoctorCheck {
                name: "network".to_string(),
                status: CheckStatus::Fail,
                detail: Some("unreachable".to_string()),
            },
        ],
        summary: DoctorSummary {
            passed: 1,
            warnings: 1,
            failed: 1,
        },
    };

    let dto = lower_doctor_report(&report);

    assert_eq!(dto.checks.len(), 3);
    // Pass with no detail (the renderer prints just the status line).
    assert_eq!(dto.checks[0].name, "config-dir");
    assert_eq!(dto.checks[0].status, CheckStatusDto::Pass);
    assert_eq!(dto.checks[0].detail, None);
    // Warn / Fail with a detail (the renderer prints the second indented line).
    assert_eq!(dto.checks[1].name, "api-key");
    assert_eq!(dto.checks[1].status, CheckStatusDto::Warn);
    assert_eq!(dto.checks[1].detail.as_deref(), Some("expires soon"));
    assert_eq!(dto.checks[2].name, "network");
    assert_eq!(dto.checks[2].status, CheckStatusDto::Fail);
    assert_eq!(dto.checks[2].detail.as_deref(), Some("unreachable"));
    // Summary tallies are carried verbatim.
    assert_eq!(dto.summary.passed, 1);
    assert_eq!(dto.summary.warnings, 1);
    assert_eq!(dto.summary.failed, 1);
}
