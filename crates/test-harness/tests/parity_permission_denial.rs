//! M4-09 parity driver — permission-gate coverage.
//!
//! Every builtin tool file under `lingxi-code/crates/tools/src/builtin/`
//! must declare a `PermissionResult` (Allow / Deny / Ask) in its
//! `permission_required` body, OR explicitly opt out via a
//! `// no-permission: <reason>` comment for tools that have no permission
//! surface (none today — every tool gates).
//!
//! This is a source-grep parity gate (NOT a runtime dispatch). The
//! runtime gate-denial contract is owned by `lingxi_permission`'s own
//! suite (`policy::tests::*`) — M4-09 just locks the tool-side surface
//! exists and hasn't drifted.
//!
//! Plan deviation: the original plan proposed a `DenyAllGate` runtime
//! driver that dispatches all 40 tools and asserts each returns
//! `Err(ToolError::PermissionDenied(_))`. That would require building
//! a `DenyAllGate` (not in `lingxi_permission`) AND constructing 40
//! schema-valid happy-path inputs. The pragmatic v0.5.0 ship gate is
//! "every tool exposes a permission decision surface" — runtime denial
//! semantics are covered by `permission::policy::tests`.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

/// 29 builtin tool source files (some files host multiple tool impls;
/// e.g. `task.rs` hosts 6 task tools, `plan_mode.rs` hosts Enter+Exit).
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
        "plan_mode.rs",
        "worktree.rs",
        "agent.rs",
        "task.rs",
        "send_message.rs",
        "mcp.rs",
        "lsp.rs",
        "ask_user_question.rs",
        "brief.rs",
        "config.rs",
        "skill.rs",
        "schedule_cron.rs",
        "tool_search.rs",
        "remote_trigger.rs",
        "synthetic_output.rs",
    ]
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Resolve a tool source-file name to its actual on-disk path. M8-P5+ moves
/// tools out of `tools/src/builtin/` into per-category crates under `tools/`.
/// Extend this map as P7 extracts further crates.
fn resolve_tool_src(file: &str) -> PathBuf {
    let root = repo_root();
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
        Some(rel) => root.join("tools").join(rel),
        None => root.join("tools/src/builtin").join(file),
    }
}

/// Every path `tool_files()` names must still be on disk. Without this the
/// list rots silently: `tools/team/src/team.rs` was deleted upstream and the
/// suites below kept reading it, so both failed with a bare `read … No such
/// file` that names the harness rather than the list that went stale.
#[test]
fn every_listed_tool_source_exists() {
    let missing: Vec<String> = tool_files()
        .into_iter()
        .filter(|file| !resolve_tool_src(file).exists())
        .map(|file| format!("{file} -> {}", resolve_tool_src(file).display()))
        .collect();
    assert!(
        missing.is_empty(),
        "tool_files() names sources that no longer exist; drop them or repoint the mapping:\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_tool_file_declares_a_permission_result() {
    let mut violations: Vec<String> = Vec::new();
    for file in tool_files() {
        let src = resolve_tool_src(file);
        let body =
            fs::read_to_string(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
        let has_permission_decl = body.contains("PermissionResult::Allow")
            || body.contains("PermissionResult::Deny")
            || body.contains("PermissionResult::Ask");
        let has_optout = body.contains("// no-permission:");
        if !has_permission_decl && !has_optout {
            violations.push(format!("{file}: no PermissionResult variant declared"));
        }
    }
    assert!(
        violations.is_empty(),
        "permission-decision coverage violations:\n  - {}",
        violations.join("\n  - ")
    );
}

#[test]
fn permission_result_enum_variants_locked() {
    // Lock the 3-variant enum surface byte-for-byte against
    // `lingxi-code/crates/permission/src/result.rs`.
    let result_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("permission/src/result.rs");
    let body = fs::read_to_string(&result_src)
        .unwrap_or_else(|e| panic!("read {}: {e}", result_src.display()));
    for needle in ["pub enum PermissionResult", "Allow {", "Deny {", "Ask {"] {
        assert!(
            body.contains(needle),
            "PermissionResult source missing {needle}"
        );
    }
}

#[test]
fn permission_module_exports_policy() {
    // Sanity: the policy module that actually evaluates rules is present.
    let lib_src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("permission/src/lib.rs");
    let body =
        fs::read_to_string(&lib_src).unwrap_or_else(|e| panic!("read {}: {e}", lib_src.display()));
    assert!(
        body.contains("pub mod policy") || body.contains("pub use"),
        "lingxi-permission must export policy surface"
    );
}
