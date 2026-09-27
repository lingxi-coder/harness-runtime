//! M4-09 parity driver — every builtin tool has its 3 lifecycle events
//! (`tengu_tool_<snake>_{started,completed,failed}`) registered in
//! `telemetry::tengu::tool::NAMES`.
//!
//! This is a registration-coverage assertion (NOT a runtime dispatch test).
//! Each covered tool is mapped here to its byte-locked snake name (the
//! same snake suffix used in production telemetry constants). The driver
//! looks each `tengu_tool_<snake>_{started,completed,failed}` string up in
//! `tengu::tool::NAMES` and fails fast if any are missing. Grep and Glob are
//! deliberately NOT in the table: claude-code v2.1.183 emits no
//! `tengu_tool_grep_*` / `tengu_tool_glob_*` telemetry, so the port emits
//! none and these tools have zero registered events.
//!
//! Cardinality note: the tool-prefixed registry contains 125 names. The extra entries beyond the
//! per-tool triads are M3-06 baseline events (`tengu_tool_started`/
//! `completed`/`failed`/`cancelled`, 4 permission events, plus a handful of
//! tool events that pre-shipped in M3-06: `web_fetch_*`, `mcp_completed`/
//! `failed`, `task_dispatched`/`completed`/`failed`, `skill_invoked`). The
//! per-tool 3-event coverage requirement still holds on top of that
//! baseline.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use telemetry::tengu::ALL_EVENT_NAMES;

/// All tool-prefixed events in `ALL_EVENT_NAMES`. Mirrors what
/// `tengu::tool::NAMES` would expose if it were `pub`.
fn tool_event_names() -> Vec<&'static str> {
    ALL_EVENT_NAMES
        .iter()
        .copied()
        .filter(|n| n.starts_with("tengu_tool_"))
        .collect()
}

/// (tool display-name, telemetry snake suffix).
const TOOL_SNAKE: &[(&str, &str)] = &[
    // File (4) — Glob is omitted: claude-code v2.1.183 emits NO
    // tengu_tool_glob_* telemetry, so the Rust port emits none either.
    ("Read", "read"),
    ("Write", "write"),
    ("Edit", "edit"),
    ("NotebookEdit", "notebook"),
    // Search (0) — Grep is omitted for the same reason (no tengu_tool_grep_*).
    // Shell (4)
    ("Bash", "bash"),
    ("PowerShell", "powershell"),
    ("REPL", "repl"),
    ("Sleep", "sleep"),
    // Web (2)
    ("WebFetch", "web_fetch"),
    ("WebSearch", "web_search"),
    // Workflow (4) — TodoWrite is omitted: claude-code 2.1.195 emits NO
    // tengu_tool_todo_write_* telemetry (D1 strict-parity removal), so the
    // Rust port emits none either.
    ("EnterPlanMode", "enter_plan_mode"),
    ("ExitPlanMode", "exit_plan_mode"),
    ("EnterWorktree", "enter_worktree"),
    ("ExitWorktree", "exit_worktree"),
    // Agent + Task (8)
    ("Agent", "agent"),
    ("TaskCreate", "task_create"),
    ("TaskGet", "task_get"),
    ("TaskList", "task_list"),
    ("TaskUpdate", "task_update"),
    ("TaskStop", "task_stop"),
    ("TaskOutput", "task_output"),
    ("SendMessage", "send_message"),
    // MCP + LSP (5)
    ("MCP", "mcp"),
    ("McpAuth", "mcp_auth"),
    ("ListMcpResourcesTool", "list_mcp_resources"),
    ("ReadMcpResourceTool", "read_mcp_resource"),
    ("LSP", "lsp"),
    // System (10)
    ("AskUserQuestion", "ask_user_question"),
    ("SendUserMessage", "brief"),
    ("Config", "config"),
    ("Skill", "skill"),
    ("CronCreate", "schedule_cron"),
    ("CronDelete", "cron_delete"),
    ("CronList", "cron_list"),
    ("ToolSearch", "tool_search"),
    ("RemoteTrigger", "remote_trigger"),
    ("StructuredOutput", "synthetic_output"),
];

#[test]
fn snake_table_covers_37_tools() {
    // Grep and Glob are intentionally absent: claude-code v2.1.183 emits NO
    // tengu_tool_grep_* / tengu_tool_glob_* telemetry, so the port emits none
    // and the coverage table drops those 2 rows (42 → 40). Strict-parity
    // (2.1.195) then drops TodoWrite (D1, no tengu_tool_todo_write_*): 40 → 39. Removing explicit team tools in 2.1.263 leaves 37.
    assert_eq!(
        TOOL_SNAKE.len(),
        37,
        "telemetry snake table must cover 37 tools (got {})",
        TOOL_SNAKE.len()
    );
    let unique: BTreeSet<&str> = TOOL_SNAKE.iter().map(|(n, _)| *n).collect();
    assert_eq!(unique.len(), 37, "tool display-names unique");
    let unique_snakes: BTreeSet<&str> = TOOL_SNAKE.iter().map(|(_, s)| *s).collect();
    assert_eq!(unique_snakes.len(), 37, "snake suffixes unique");
}

#[test]
fn every_tool_has_three_registered_events() {
    let registered: BTreeSet<&str> = tool_event_names().into_iter().collect();
    let mut missing: Vec<String> = Vec::new();
    for (display, snake) in TOOL_SNAKE {
        for suffix in ["started", "completed", "failed"] {
            let expected = format!("tengu_tool_{snake}_{suffix}");
            if !registered.contains(expected.as_str()) {
                missing.push(format!("{display}: {expected}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "tengu::tool::NAMES missing the following per-tool events:\n  - {}",
        missing.join("\n  - ")
    );
}

#[test]
fn names_cardinality_locked() {
    // M3-06 baseline + M4-02..08 deltas, minus the 6 fabricated grep/glob
    // events (claude-code v2.1.183 emits no tengu_tool_grep_* /
    // tengu_tool_glob_* telemetry), land at 134 entries. Strict-parity (2.1.195)
    // then drops the 3 tengu_tool_todo_write_* events (D1): 134 - 3 = 131.
    let count = tool_event_names().len();
    assert_eq!(
        count, 125,
        "tengu_tool_* events in ALL_EVENT_NAMES locked at 125 entries (Grep/Glob/TodoWrite emit no telemetry; explicit team tools removed in 2.1.263)"
    );
}

#[test]
fn completed_suffix_lock_no_succeeded_drift() {
    // Tool events must use `_completed`, never `_succeeded`. (Non-tool
    // subsystems like api use `_succeeded` by design — only the
    // `tengu_tool_*` namespace is locked here.)
    for n in tool_event_names() {
        assert!(
            !n.contains("_succeeded"),
            "found _succeeded suffix in tool event {n}; must use _completed"
        );
    }
}
