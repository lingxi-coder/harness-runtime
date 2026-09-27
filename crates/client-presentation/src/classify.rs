//! Port of `getToolSearchOrReadInfo` (`utils/collapseReadSearch.ts:143`).
//!
//! Fullscreen-only branches (`isFullscreenEnvEnabled()`) are selected by the
//! caller's live terminal mode. Inline mode keeps native terminal scrollback
//! semantics; fullscreen additionally folds Snip / ToolSearch, memory writes,
//! and non-search Bash commands. MCP folds only when the surfaced MCP tool is
//! itself a read/search/list primitive; mutating MCP calls stay visible.

use serde_json::Value;
use tool_shell::search_read::{is_search_or_read_command, ReadSearchKind};

/// Classification of one tool use (`SearchOrReadResult` in the reference).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchOrReadResult {
    /// Folds into a collapsed group (does not break the run).
    pub is_collapsible: bool,
    /// Search (Grep/Glob or bash grep/rg/find).
    pub is_search: bool,
    /// Read (Read or bash cat/head/tail).
    pub is_read: bool,
    /// Directory listing (bash ls/tree/du).
    pub is_list: bool,
    /// REPL wrapper — absorbed with no count.
    pub is_repl: bool,
    /// Meta-op absorbed with no count or standalone visible summary.
    pub is_absorbed_silently: bool,
    /// Fullscreen-only: a non-search/read Bash command ("Ran N bash commands").
    pub is_bash: bool,
    /// Fullscreen-only: the normalized MCP server name to summarize.
    pub mcp_server_name: Option<String>,
    /// An MCP call. Kept separately because the generic dispatcher may not
    /// carry a parseable full name even though the invocation still folds.
    pub is_mcp: bool,
    /// Fullscreen-only: a Write/Edit targeting an auto-managed memory file.
    pub is_memory_write: bool,
}

/// The `command` string of a Bash tool input, if present.
fn bash_command(input: &Value) -> Option<&str> {
    input.get("command").and_then(Value::as_str)
}

fn mcp_server_name(tool: &str, input: &Value) -> Option<String> {
    let full_name = if tool == "MCP" {
        input.get("full_name").and_then(Value::as_str)?
    } else {
        tool
    };
    full_name
        .strip_prefix("mcp__")?
        .split_once("__")
        .map(|(server, _)| server.to_string())
}

fn mcp_tool_name<'a>(tool: &'a str, input: &'a Value) -> Option<&'a str> {
    let full_name = if tool == "MCP" {
        input.get("full_name").and_then(Value::as_str)?
    } else {
        tool
    };
    let rest = full_name.strip_prefix("mcp__")?;
    let (_, tool_name) = rest.split_once("__")?;
    Some(tool_name)
}

const MCP_MUTATING_TOKENS: &[&str] = &[
    "add",
    "append",
    "apply",
    "approve",
    "archive",
    "assign",
    "attach",
    "cancel",
    "close",
    "commit",
    "copy",
    "create",
    "delete",
    "deploy",
    "destroy",
    "disable",
    "edit",
    "enable",
    "execute",
    "insert",
    "install",
    "invite",
    "mark",
    "merge",
    "move",
    "mutate",
    "patch",
    "post",
    "publish",
    "reject",
    "remove",
    "rename",
    "reopen",
    "reply",
    "resolve",
    "run",
    "send",
    "set",
    "start",
    "stop",
    "submit",
    "trigger",
    "uninstall",
    "update",
    "upload",
    "upsert",
    "write",
];
const MCP_SEARCH_TOKENS: &[&str] = &["find", "glob", "grep", "lookup", "query", "search"];
const MCP_READ_TOKENS: &[&str] = &[
    "describe", "fetch", "get", "inspect", "read", "retrieve", "show", "view",
];
const MCP_LIST_TOKENS: &[&str] = &["enumerate", "list", "ls", "tree"];

fn classify_mcp_tool(tool: &str, input: &Value) -> Option<SearchOrReadResult> {
    let tool_name = mcp_tool_name(tool, input)?;
    let normalized = tool_name.replace(['_', '-', '.'], " ").to_ascii_lowercase();
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.is_empty()
        || tokens
            .iter()
            .any(|token| MCP_MUTATING_TOKENS.contains(token))
    {
        return None;
    }
    let is_search = tokens.iter().any(|token| MCP_SEARCH_TOKENS.contains(token));
    let is_read = tokens.iter().any(|token| MCP_READ_TOKENS.contains(token));
    let is_list = tokens.iter().any(|token| MCP_LIST_TOKENS.contains(token));
    if !(is_search || is_read || is_list) {
        return None;
    }
    Some(SearchOrReadResult {
        is_collapsible: true,
        mcp_server_name: mcp_server_name(tool, input),
        is_mcp: true,
        ..Default::default()
    })
}

fn is_auto_managed_memory_write(tool: &str, input: &Value) -> bool {
    if !matches!(tool, "Write" | "Edit") {
        return false;
    }
    let Some(path) = input.get("file_path").and_then(Value::as_str) else {
        return false;
    };
    let path = path.replace('\\', "/");
    path.contains("/.lingxi/memdir/")
        || path.contains("/.lingxi/team-mem/")
        || ((path.contains("/.lingxi/projects/") || path.contains("/.claude/projects/"))
            && path.contains("/memory/"))
}

/// Classify one tool use by name + raw JSON input (`getToolSearchOrReadInfo`).
#[must_use]
pub fn classify(tool: &str, input: &Value, fullscreen: bool) -> SearchOrReadResult {
    // REPL is absorbed silently — its inner tool calls flow through separately
    // as regular Read/Grep/Bash uses. The wrapper contributes no count and does
    // not break the group.
    if tool == "REPL" {
        return SearchOrReadResult {
            is_collapsible: true,
            is_repl: true,
            is_absorbed_silently: true,
            ..Default::default()
        };
    }
    if fullscreen && is_auto_managed_memory_write(tool, input) {
        return SearchOrReadResult {
            is_collapsible: true,
            is_memory_write: true,
            ..Default::default()
        };
    }
    if fullscreen && matches!(tool, "Snip" | "ToolSearch") {
        return SearchOrReadResult {
            is_collapsible: true,
            is_absorbed_silently: true,
            ..Default::default()
        };
    }
    if tool == "MCP" || tool.starts_with("mcp__") {
        return classify_mcp_tool(tool, input).unwrap_or_default();
    }
    let base: ReadSearchKind = is_search_or_read_command(tool, bash_command(input));
    let core = base.is_collapsible();
    // Under fullscreen, a non-search/read Bash command is its own collapsible
    // category ("Ran N bash commands") instead of breaking the group.
    let is_bash = fullscreen && !core && tool == "Bash";
    SearchOrReadResult {
        is_collapsible: core || is_bash,
        is_search: base.is_search,
        is_read: base.is_read,
        is_list: base.is_list,
        is_bash,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_grep_glob_collapse() {
        assert!(classify("Read", &json!({"file_path": "a.rs"}), false).is_read);
        assert!(classify("Grep", &json!({"pattern": "x"}), false).is_search);
        assert!(classify("Glob", &json!({"pattern": "*.rs"}), false).is_search);
    }

    #[test]
    fn repl_is_silent_absorb() {
        let r = classify("REPL", &json!({}), false);
        assert!(r.is_collapsible && r.is_repl && r.is_absorbed_silently);
    }

    #[test]
    fn bash_cat_reads_bash_rm_breaks() {
        assert!(classify("Bash", &json!({"command": "cat a.rs"}), false).is_read);
        assert!(!classify("Bash", &json!({"command": "rm a.rs"}), false).is_collapsible);
        let fullscreen = classify("Bash", &json!({"command": "rm a.rs"}), true);
        assert!(fullscreen.is_collapsible && fullscreen.is_bash);
    }

    #[test]
    fn edit_write_are_not_collapsible() {
        assert!(!classify("Edit", &json!({"file_path": "a"}), false).is_collapsible);
        assert!(!classify("Write", &json!({"file_path": "a"}), false).is_collapsible);
    }

    #[test]
    fn fullscreen_only_meta_mcp_and_memory_branches_are_reachable() {
        for tool in ["Snip", "ToolSearch"] {
            let inline = classify(tool, &json!({}), false);
            assert!(!inline.is_collapsible);
            let full = classify(tool, &json!({}), true);
            assert!(full.is_collapsible && full.is_absorbed_silently);
        }

        let mcp = classify("mcp__filesystem__read_file", &json!({}), false);
        assert!(mcp.is_collapsible && mcp.is_mcp);
        assert!(!mcp.is_read && !mcp.is_search && !mcp.is_list);
        assert_eq!(mcp.mcp_server_name.as_deref(), Some("filesystem"));
        let mcp_list = classify("mcp__filesystem__list_dir", &json!({}), false);
        assert!(mcp_list.is_collapsible && mcp_list.is_mcp);
        let mcp_search = classify("mcp__github__search_code", &json!({}), false);
        assert!(mcp_search.is_collapsible && mcp_search.is_mcp);
        let mutating = classify("mcp__github__create_issue", &json!({}), false);
        assert!(!mutating.is_collapsible && !mutating.is_mcp);
        let mixed_mutating = classify("mcp__mail__archive_and_list", &json!({}), false);
        assert!(
            !mixed_mutating.is_collapsible && !mixed_mutating.is_mcp,
            "a read-like suffix must never hide a mutating MCP operation"
        );
        let generic_mutating = classify(
            "MCP",
            &json!({"full_name": "mcp__slack__send_message"}),
            false,
        );
        assert!(!generic_mutating.is_collapsible && !generic_mutating.is_mcp);
        let generic = classify(
            "MCP",
            &json!({"full_name": "mcp__slack__search_messages"}),
            false,
        );
        assert!(generic.is_collapsible && generic.is_mcp);
        assert_eq!(generic.mcp_server_name.as_deref(), Some("slack"));

        let memory = classify(
            "Write",
            &json!({"file_path": "/home/u/.lingxi/memdir/note.md"}),
            true,
        );
        assert!(memory.is_collapsible && memory.is_memory_write);
    }
}
