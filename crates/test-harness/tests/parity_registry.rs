//! Registry parity driver — locks the Claude Code 2.1.263 base builtin surface.
//!
//! Asserts that the fixture `registry_42_tools.json` declares exactly 40
//! tools across 8 categories AND that each declared name is a known
//! `*_TOOL_NAME` constant in production (`tools::builtin`).
//!
//! This is a byte-lock parity driver in the M4-01..08 style: it does NOT
//! dispatch tools at runtime (that machinery lives behind `dummy_ctx` in
//! the `lingxi-tools` crate's `#[cfg(test)]` block — see
//! `register_all_inserts_forty_tools_after_m4_08`). Instead, this driver
//! cross-checks the fixture against the per-tool `TOOL_NAME` constants
//! exposed by each builtin module. Capability-driven MCP resource tools are
//! tested in `tool-mcp`; they are intentionally not part of this base list.

#![allow(clippy::unwrap_used)]

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use test_harness::parity::load_fixture;

#[derive(Deserialize)]
struct Fixture {
    total: usize,
    by_category: BTreeMap<String, Vec<String>>,
}

fn fx() -> Fixture {
    load_fixture::<Fixture>("registry_42_tools")
}

#[test]
fn registry_42_tools_fixture_totals_to_40() {
    let f = fx();
    assert_eq!(f.total, 40, "fixture declares total=40");
    let summed: usize = f.by_category.values().map(Vec::len).sum();
    assert_eq!(
        summed, 40,
        "sum of by_category lengths is 40 (got {summed})"
    );
    let unique: BTreeSet<&String> = f.by_category.values().flat_map(|v| v.iter()).collect();
    assert_eq!(unique.len(), 40, "by_category names are unique");
}

#[test]
fn registry_42_tools_fixture_categories_locked() {
    let f = fx();
    let cats: BTreeSet<&str> = f.by_category.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = [
        "file", "search", "shell", "web", "workflow", "agent", "mcp_lsp", "system",
    ]
    .into_iter()
    .collect();
    assert_eq!(cats, expected, "8-category lock");

    let lens: BTreeMap<&str, usize> = f
        .by_category
        .iter()
        .map(|(k, v)| (k.as_str(), v.len()))
        .collect();
    assert_eq!(lens["file"], 5);
    assert_eq!(lens["search"], 1);
    assert_eq!(lens["shell"], 4);
    assert_eq!(lens["web"], 2);
    assert_eq!(lens["workflow"], 5);
    // 9 since 2.1.232 (56760bb1a): ListAgents joins the SendMessage it discovers.
    assert_eq!(lens["agent"], 9);
    assert_eq!(lens["mcp_lsp"], 3);
    assert_eq!(lens["system"], 11);
}

/// Cross-check the fixture's 40 names against the production
/// `*_TOOL_NAME` constants (when an explicit named constant exists) and
/// the in-module `TOOL_NAME` constants (single-tool modules).
#[test]
fn fixture_names_match_production_constants() {
    let f = fx();
    let names: BTreeSet<String> = f
        .by_category
        .values()
        .flat_map(|v| v.iter().cloned())
        .collect();

    // Each name below must appear as a `pub const … &str`
    // in its module — byte-for-byte equal to the fixture entry.
    let pairs: &[(&str, &str)] = &[
        // File (5)
        ("Read", tool_file::read::TOOL_NAME),
        ("Write", tool_file::write::TOOL_NAME),
        ("Edit", tool_file::edit::TOOL_NAME),
        ("NotebookEdit", tool_file::notebook_edit::TOOL_NAME),
        ("Glob", tool_file::glob::TOOL_NAME),
        // Search (1)
        ("Grep", tool_file::grep::TOOL_NAME),
        // Shell (4)
        ("Bash", tool_shell::bash::TOOL_NAME),
        ("PowerShell", tool_shell::powershell::TOOL_NAME),
        ("REPL", tool_shell::repl::TOOL_NAME),
        ("Sleep", tool_ui::sleep::TOOL_NAME),
        // Web (2)
        ("WebFetch", tool_web::web_fetch::TOOL_NAME),
        ("WebSearch", tool_web::web_search::TOOL_NAME),
        // Workflow (5)
        ("TodoWrite", tool_task::todo_write::TOOL_NAME),
        ("EnterPlanMode", tool_plan::plan_mode::ENTER_TOOL_NAME),
        ("ExitPlanMode", tool_plan::plan_mode::EXIT_TOOL_NAME),
        ("EnterWorktree", tool_worktree::worktree::ENTER_TOOL_NAME),
        ("ExitWorktree", tool_worktree::worktree::EXIT_TOOL_NAME),
        // Agent + Task (8)
        ("Agent", tool_agent::agent::AGENT_TOOL_NAME),
        ("TaskCreate", tool_task::task::TASK_CREATE_TOOL_NAME),
        ("TaskGet", tool_task::task::TASK_GET_TOOL_NAME),
        ("TaskList", tool_task::task::TASK_LIST_TOOL_NAME),
        ("TaskUpdate", tool_task::task::TASK_UPDATE_TOOL_NAME),
        ("TaskStop", tool_task::task::TASK_STOP_TOOL_NAME),
        ("TaskOutput", tool_task::task::TASK_OUTPUT_TOOL_NAME),
        ("SendMessage", tool_ui::send_message::SEND_MESSAGE_TOOL_NAME),
        ("ListAgents", tool_ui::list_agents::LIST_AGENTS_TOOL_NAME),
        // MCP + LSP (3). The generic dispatcher remains an internal
        // compatibility constructor, not a production builtin. Resource tools
        // are capability-driven, not base builtins.
        ("McpAuth", tool_mcp::mcp_tool::MCP_AUTH_TOOL_NAME),
        (
            "WaitForMcpServers",
            tool_mcp::wait_for_mcp_servers::WAIT_FOR_MCP_SERVERS_TOOL_NAME,
        ),
        ("LSP", tool_lsp::lsp_tool::LSP_TOOL_NAME),
        // System (11)
        (
            "AskUserQuestion",
            tool_ui::ask_user_question::ASK_USER_QUESTION_TOOL_NAME,
        ),
        ("SendUserMessage", tool_ui::brief::BRIEF_TOOL_NAME),
        ("Config", tool_meta::config::CONFIG_TOOL_NAME),
        ("Skill", tool_skill::skill::SKILL_TOOL_NAME),
        (
            "CronCreate",
            tool_cron::schedule_cron::CRON_CREATE_TOOL_NAME,
        ),
        ("CronDelete", tool_cron::cron_delete::CRON_DELETE_TOOL_NAME),
        ("CronList", tool_cron::cron_list::CRON_LIST_TOOL_NAME),
        ("ToolSearch", tool_meta::tool_search::TOOL_SEARCH_TOOL_NAME),
        (
            "RemoteTrigger",
            tool_cron::remote_trigger::REMOTE_TRIGGER_TOOL_NAME,
        ),
        (
            "StructuredOutput",
            tool_ui::synthetic_output::SYNTHETIC_OUTPUT_TOOL_NAME,
        ),
        (
            "ReportFindings",
            tool_ui::report_findings::REPORT_FINDINGS_TOOL_NAME,
        ),
    ];

    assert_eq!(pairs.len(), 40, "production-constant lock covers 40 tools");

    for (fixture_name, production_const) in pairs {
        assert_eq!(
            production_const, fixture_name,
            "tool {fixture_name}: production constant ({production_const}) diverges from fixture"
        );
        assert!(
            names.contains(*fixture_name),
            "fixture missing expected name {fixture_name}"
        );
    }

    // Final sanity: the paired names exactly match the fixture set.
    let paired: BTreeSet<String> = pairs.iter().map(|(n, _)| (*n).to_string()).collect();
    assert_eq!(paired, names, "fixture and production-constant sets agree");
}

#[test]
fn explicit_team_management_tools_are_absent() {
    let f = fx();
    for name in f.by_category.values().flatten() {
        assert!(!matches!(name.as_str(), "TeamCreate" | "TeamDelete"));
    }
}
