//! M4-09 parity driver — every builtin tool that produces variable-length
//! output routes it through `tools::shared::truncate` (or the
//! tool_trait-level `OutputTruncated` enforcement), with the
//! `MAX_TOOL_OUTPUT_LENGTH = 30_000` and the locked suffix.
//!
//! Two assertions:
//!   1. Constant lock — the two literals are exactly the spec values.
//!   2. Source-level lock — every per-tool source file either references
//!      `MAX_TOOL_OUTPUT_LENGTH` / `truncate_shell_output` / a tool-specific
//!      truncator, or carries an explicit `// no-truncation: <reason>` opt-out
//!      for tools whose return is bounded by construction (e.g. `{ ok: bool }`).

#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use tool_api::util::output_truncation::{MAX_TOOL_OUTPUT_LENGTH, SHELL_TRUNCATION_SUFFIX_TEMPLATE};

/// Each entry MUST carry a one-line justification.
fn exempt_tools() -> HashSet<&'static str> {
    [
        // Pure side-effect or fixed-shape return:
        "exit_plan_mode", // returns empty object (handled in plan_mode.rs alongside enter)
        "exit_worktree",  // returns empty object (handled in worktree.rs alongside enter)
        "sleep",          // returns { slept_ms: u64 }
        "schedule_cron",  // returns { id, humanSchedule, recurring, durable }
        "cron_delete",    // returns { id } (fixed shape)
        "cron_list", // returns a bounded jobs array (<= MAX_JOBS=50); max_result_size_chars caps it
        "task_stop", // returns { stopped: bool, task_id }
        "task_update", // returns { task_id, status }
        "team_delete", // returns { deleted: bool }
        "mcp_auth",  // returns { authenticated: bool }
        "remote_trigger", // returns { stub: true, ... }
    ]
    .into_iter()
    .collect()
}

/// Mapping: tool display-name → source file under `builtin/`. Some tool
/// modules host multiple display-names (e.g. `task.rs` hosts 6 task tools,
/// `plan_mode.rs` hosts Enter+Exit). The grep is at file granularity.
/// Resolve a tool source-file name to its actual on-disk path. M8-P5+ moves
/// tools out of `tools/src/builtin/` into per-category crates under `tools/`,
/// so the file-tool names map to `tools/file/src/<renamed>.rs`. Extend this
/// map as P7 extracts further crates.
fn resolve_tool_src(repo_root: &std::path::Path, file: &str) -> PathBuf {
    let mapped: Option<&str> = match file {
        "file_read.rs" => Some("file/src/read.rs"),
        "file_write.rs" => Some("file/src/write.rs"),
        "file_edit.rs" => Some("file/src/edit.rs"),
        "notebook_edit.rs" => Some("file/src/notebook_edit.rs"),
        "glob.rs" => Some("file/src/glob.rs"),
        "grep.rs" => Some("file/src/grep.rs"),
        "bash.rs" => Some("shell/src/bash.rs"),
        "powershell.rs" => Some("shell/src/powershell.rs"),
        "repl.rs" => Some("shell/src/repl.rs"),
        "task.rs" => Some("task/src/task.rs"),
        "todo_write.rs" => Some("task/src/todo_write.rs"),
        "web_fetch.rs" => Some("web/src/web_fetch.rs"),
        "web_search.rs" => Some("web/src/web_search.rs"),
        "plan_mode.rs" => Some("plan/src/plan_mode.rs"),
        "config.rs" => Some("meta/src/config.rs"),
        "tool_search.rs" => Some("meta/src/tool_search.rs"),
        "schedule_cron.rs" => Some("cron/src/schedule_cron.rs"),
        "cron_delete.rs" => Some("cron/src/cron_delete.rs"),
        "cron_list.rs" => Some("cron/src/cron_list.rs"),
        "remote_trigger.rs" => Some("cron/src/remote_trigger.rs"),
        "ask_user_question.rs" => Some("ui/src/ask_user_question.rs"),
        "brief.rs" => Some("ui/src/brief.rs"),
        "send_message.rs" => Some("ui/src/send_message.rs"),
        "sleep.rs" => Some("ui/src/sleep.rs"),
        "synthetic_output.rs" => Some("ui/src/synthetic_output.rs"),
        "skill.rs" => Some("skill/src/skill.rs"),
        "worktree.rs" => Some("worktree/src/worktree.rs"),
        "lsp.rs" => Some("lsp/src/lsp_tool.rs"),
        "mcp.rs" => Some("mcp/src/mcp_tool.rs"),
        "agent.rs" => Some("agent/src/agent.rs"),
        _ => None,
    };
    match mapped {
        Some(rel) => repo_root.join("tools").join(rel),
        None => repo_root.join("tools/src/builtin").join(file),
    }
}

fn tool_files() -> Vec<&'static str> {
    vec![
        "file_read.rs",
        "file_write.rs",
        "file_edit.rs",
        "notebook_edit.rs",
        "glob.rs",
        "grep.rs",
        "bash.rs",
        "powershell.rs",
        "repl.rs",
        "sleep.rs",
        "web_fetch.rs",
        "web_search.rs",
        "todo_write.rs",
        "plan_mode.rs", // Enter+Exit
        "worktree.rs",  // Enter+Exit
        "agent.rs",
        "task.rs", // 6 task tools
        "send_message.rs",
        "mcp.rs", // MCP + McpAuth + ListMcpResources + ReadMcpResource
        "lsp.rs",
        "ask_user_question.rs",
        "brief.rs",
        "config.rs",
        "skill.rs",
        "schedule_cron.rs",
        "cron_delete.rs",
        "cron_list.rs",
        "tool_search.rs",
        "remote_trigger.rs",
        "synthetic_output.rs",
    ]
}

/// Every path `tool_files()` names must still be on disk. Without this the
/// list rots silently: `tools/team/src/team.rs` was deleted upstream and the
/// suites below kept reading it, so both failed with a bare `read … No such
/// file` that names the harness rather than the list that went stale.
#[test]
fn every_listed_tool_source_exists() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let missing: Vec<String> = tool_files()
        .into_iter()
        .filter(|file| !resolve_tool_src(&root, file).exists())
        .map(|file| format!("{file} -> {}", resolve_tool_src(&root, file).display()))
        .collect();
    assert!(
        missing.is_empty(),
        "tool_files() names sources that no longer exist; drop them or repoint the mapping:\n{}",
        missing.join("\n")
    );
}

#[test]
fn output_truncation_constants_locked() {
    assert_eq!(
        MAX_TOOL_OUTPUT_LENGTH, 30_000,
        "BASH_MAX_OUTPUT_LENGTH default: 30_000 chars"
    );
    assert_eq!(
        SHELL_TRUNCATION_SUFFIX_TEMPLATE, "\n\n... [{N} lines truncated] ...",
        "claude-code Qyu()/BashTool utils.ts:156-158 shell truncation suffix"
    );
}

#[test]
fn truncation_lib_source_contains_literals() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let lib_src = repo_root.join("tool-api/src/util/output_truncation.rs");
    let body =
        fs::read_to_string(&lib_src).unwrap_or_else(|e| panic!("read {}: {e}", lib_src.display()));
    assert!(
        body.contains("30_000"),
        "MAX_TOOL_OUTPUT_LENGTH literal 30_000 missing from {}",
        lib_src.display()
    );
    assert!(
        body.contains("... [{N} lines truncated] ..."),
        "SHELL_TRUNCATION_SUFFIX_TEMPLATE literal missing from {}",
        lib_src.display()
    );
}

#[test]
fn every_tool_with_output_calls_truncate_or_opts_out() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let exempt = exempt_tools();

    let mut violations: Vec<String> = Vec::new();
    for file in tool_files() {
        let src = resolve_tool_src(&repo_root, file);
        let body =
            fs::read_to_string(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
        let has_call = body.contains("MAX_TOOL_OUTPUT_LENGTH")
            || body.contains("output_truncation::truncate")
            || body.contains("shared::truncate")
            || body.contains("truncate_shell_output")
            || body.contains("OutputTruncated")
            // The registry enforces a tool's explicit result-size method even
            // when the implementation does not call a truncation helper.
            || body.contains("fn max_result_size_chars")
            // ST-10: a tool may declare its OWN cap instead of reusing the
            // shared `MAX_TOOL_OUTPUT_LENGTH`, because upstream the caps differ
            // per tool (Glob 100_000 vs Grep 20_000). Declaring
            // `<TOOL>_MAX_RESULT_SIZE_CHARS` and returning it from
            // `max_result_size_chars` is still truncation coverage — the
            // framework enforces the value — so it satisfies this check.
            || body.contains("_MAX_RESULT_SIZE_CHARS");
        // Files in the exempt set are allowed without a call (their tools
        // return bounded outputs). Their stem (without `.rs`) appears in
        // `exempt_tools()`.
        let stem = file.trim_end_matches(".rs");
        let is_exempt = exempt.contains(stem);
        let has_optout_marker =
            body.contains("// no-truncation:") || body.contains("//! no-truncation:");
        if !has_call && !is_exempt && !has_optout_marker {
            violations.push(format!(
                "{file}: no truncation call AND not in exempt set AND no `// no-truncation:` opt-out"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "output-truncation coverage violations:\n  - {}",
        violations.join("\n  - ")
    );
}
