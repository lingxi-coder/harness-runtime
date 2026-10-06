//! Per-tool default Y/N decisions for the interactive permission prompt.
//!
//! Source-of-truth table — see M5-05 plan §"Tool default Y/N table" for the
//! claude-code references that justify each row. Unknown tool names default
//! to [`PromptDefault::DenyByDefault`] (fail-closed).
//!
//! Aggregate, oracle-parity set: 14 `DenyByDefault` (destructive / external
//! side-effects), 32 `AllowByDefault` (read-only, agent-local, or — the 2.1.270
//! re-audit — a tool whose oracle object declares no `checkPermissions` and so
//! defaults to `{behavior:"allow"}`) = 46 tools, plus one synthetic
//! `<unknown>` fallback.
//!
//! LINGXI DIVERGENCE: `Workflow`, a row with no oracle counterpart, reported by
//! [`is_divergence_tool`]. claude-code gates it behind the `WORKFLOW_SCRIPTS`
//! feature and it is absent from external builds (see `mode_policy`'s module
//! doc), so the M5-05 table has no row for it and its default here is a LingXi
//! decision, NOT oracle parity. It is `AllowByDefault` (the hand-off).
//!
//! A product that ships host-owned tools of its own adds their rows with
//! [`install_tool_default_extension`]; those rows are divergence rows too, and
//! the product keeps its own counts and its own split by reversibility.
//!
//! 🚨 `AllowByDefault` is not merely a prompt default: it also short-circuits
//! the Plan-mode mutation backstop and the `DontAsk` ask→deny transform. Every
//! divergence row is therefore EXCLUDED from the Plan-mode auto-allow unless it
//! is plan-safe — see `policy_gate::read_only_default_auto_allows`, which keys
//! that carve-out on [`is_divergence_tool`].
//!
//! The base counts are asserted in
//! `tests::the_counts_in_this_module_doc_are_the_counts_in_the_table`, so
//! additions cannot silently desync the documentation from the table.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::gate::PromptDefault;

static TOOL_DEFAULTS: OnceLock<HashMap<&'static str, PromptDefault>> = OnceLock::new();

/// What a product adds to the table: rows for its own host-owned tools, the ones of those that are safe to
/// run while the user is only planning, and how a permission rule's content is read from a call.
///
/// Installed once at boot ([`install_tool_default_extension`]); the table itself is already process-wide.
/// A tool the product never installed is an unknown tool, and an unknown tool is `DenyByDefault`.
pub struct ToolDefaultExtension {
    /// The product's rows, keyed by tool name.
    pub rows: HashMap<&'static str, PromptDefault>,
    /// The rows that may run in Plan mode.
    pub plan_safe: &'static [&'static str],
    /// The content a permission rule is matched against for one of the product's tools
    /// (`Tool(content)`); `None` for any other tool and for a call that names none.
    pub rule_content: fn(tool_name: &str, input: &serde_json::Value) -> Option<String>,
}

static EXTENSION: OnceLock<ToolDefaultExtension> = OnceLock::new();

/// Install the product's rows. The first call wins; later calls are ignored and report `false`, so
/// every composition root may call it.
pub fn install_tool_default_extension(extension: ToolDefaultExtension) -> bool {
    EXTENSION.set(extension).is_ok()
}

pub(crate) fn extension() -> Option<&'static ToolDefaultExtension> {
    EXTENSION.get()
}

/// The content a rule is matched against for an installed product tool.
pub(crate) fn extension_rule_content(name: &str, input: &serde_json::Value) -> Option<String> {
    (extension()?.rule_content)(name, input)
}

fn init_defaults() -> HashMap<&'static str, PromptDefault> {
    use PromptDefault::{AllowByDefault, DenyByDefault};
    let mut m: HashMap<&'static str, PromptDefault> = HashMap::with_capacity(48);

    // Allow-by-default tools ([Y/n]) — 32 entries. (It said 20 while there
    // were 21, from before `ListAgents` was added; the count is asserted in
    // `tests::the_counts_in_this_module_doc_are_the_counts_in_the_table` now.)
    m.insert("Agent", AllowByDefault);
    m.insert("AskUserQuestion", AllowByDefault);
    m.insert("Config", AllowByDefault);
    m.insert("CronList", AllowByDefault); // read-only: lists local scheduled jobs
    m.insert("EnterPlanMode", AllowByDefault);
    m.insert("ExitPlanMode", AllowByDefault);
    m.insert("Glob", AllowByDefault);
    m.insert("Grep", AllowByDefault);
    m.insert("LSP", AllowByDefault);
    // 2.1.232 `zy`. Read-only discovery, and its own `check_permissions`
    // answers Allow — but a tool ABSENT from this table falls to the
    // fail-closed `DenyByDefault`, which makes `read_only_default_auto_allows`
    // false and raises a prompt on every call. It was missed when the registry
    // count went 42 -> 43: the oracle side of that count still balanced only
    // because the legacy `Task` alias below occupies a slot the registry does
    // not have.
    m.insert("ListAgents", AllowByDefault);
    m.insert("Read", AllowByDefault);
    m.insert("SendUserMessage", AllowByDefault); // wire name of BriefTool (Brief alias)
    m.insert("Skill", AllowByDefault);
    m.insert("Sleep", AllowByDefault);
    m.insert("StructuredOutput", AllowByDefault);
    m.insert("Task", AllowByDefault); // legacy alias of Agent
    m.insert("TaskGet", AllowByDefault);
    m.insert("TaskList", AllowByDefault);
    m.insert("TaskOutput", AllowByDefault);
    m.insert("TodoWrite", AllowByDefault);
    m.insert("ToolSearch", AllowByDefault);
    // ---- 2.1.270 re-audit: tools whose ORACLE default is `allow` ------------
    // The oracle's tool factory supplies the default every tool object that
    // declares no `checkPermissions` of its own gets:
    //
    // ```js
    // checkPermissions:(n,a)=>{let{tool:r,call:i}=t(a);
    //   return r.checkPermissions?r.checkPermissions(n,i)
    //                            :Promise.resolve({behavior:"allow",updatedInput:n})}
    // ```
    // (`At`, src_167837625.js @8300). Only a `passthrough` reaches `tBn`'s mode
    // tail (`C.behavior==="passthrough"?{...C,behavior:"ask",…}:C`), so a tool
    // with no `checkPermissions` NEVER prompts on mode alone — deny/ask rules,
    // `requiresUserInteraction` and the MCP ceiling still bind above it.
    //
    // Scanning every `At({…})` literal in the 2.1.270 binary for the presence
    // of a `checkPermissions` key gives the authoritative split. These eleven
    // rows were on the wrong side of it: each of the tool objects below
    // declares none, so upstream allows them outright, while this table's
    // fail-closed `DenyByDefault` (or missing row) made
    // `read_only_default_auto_allows` false and raised a prompt on every call.
    // `TaskCreate` is the one users hit constantly — "create task" prompted in
    // every mode, Auto included.
    m.insert("TaskCreate", AllowByDefault); // `uw`, no checkPermissions
    m.insert("TaskUpdate", AllowByDefault); // `pw`, no checkPermissions
    m.insert("TaskStop", AllowByDefault); // `Kg`, no checkPermissions
    m.insert("CronDelete", AllowByDefault); // `cw`, no checkPermissions
    m.insert("ExitWorktree", AllowByDefault); // `ile`, no checkPermissions
    m.insert("ListMcpResourcesTool", AllowByDefault); // `X5`, no checkPermissions
    m.insert("ReadMcpResourceTool", AllowByDefault); // `AW`, no checkPermissions

    // Rows this table never had at all, so they fell to the fail-closed
    // `DenyByDefault` that `tool_default` returns for an unknown name.
    m.insert("ReadMcpResourceDirTool", AllowByDefault); // `vW`
    m.insert("WaitForMcpServers", AllowByDefault); // `eD`
    m.insert("ReportFindings", AllowByDefault); // `t0`
    m.insert("PushNotification", AllowByDefault); // `UR`

    // Deny-by-default tools ([y/N]) — 14 entries.
    m.insert("Bash", DenyByDefault);
    m.insert("Edit", DenyByDefault);
    m.insert("EnterWorktree", DenyByDefault);
    m.insert("MCP", DenyByDefault);
    m.insert("McpAuth", DenyByDefault);
    m.insert("NotebookEdit", DenyByDefault);
    m.insert("PowerShell", DenyByDefault);
    m.insert("REPL", DenyByDefault);
    m.insert("RemoteTrigger", DenyByDefault);
    m.insert("CronCreate", DenyByDefault);
    m.insert("SendMessage", DenyByDefault);
    m.insert("WebFetch", DenyByDefault);
    m.insert("WebSearch", DenyByDefault);
    m.insert("Write", DenyByDefault);

    // ---- LINGXI DIVERGENCE: the workflow-script launcher -------------------
    // NOT oracle parity: claude-code gates `Workflow` behind `WORKFLOW_SCRIPTS`
    // and external builds never advertise it (`mode_policy` module doc), so the
    // M5-05 table has no row to copy. Without a row `tool_default` fell through
    // to the fail-closed `DenyByDefault`, which prompted [y/N] on the single
    // hand-off of the whole create-app flow — unlike its siblings `Agent` and
    // `Skill`.
    //
    // Two disjoint launch shapes, both already gated below this row
    // (`WorkflowTool::check_permissions`, tools/workflow/src/lib.rs:1115):
    //   - a `scriptPath` launch is re-asked as the canonical `Read` tool on the
    //     resolved file, so a `Read(...)` rule and the symlink checks apply;
    //   - an inline / named launch reads no caller-selected file and self-allows
    //     with the reason "Workflow launch — spawned agents are individually
    //     permissioned".
    // They are the `else` and the `Some` arms of one `let`, so exactly one runs
    // per call; neither leaves the launch unchecked.
    //
    // `Workflow` is NOT plan-safe (`mode_policy::PLAN_SAFE_TOOLS`), so
    // [`is_divergence_tool`] keeps Plan mode prompting for it.
    m.insert("Workflow", AllowByDefault);

    // 46 oracle-parity tools + the `Workflow` divergence row. A product adds its own rows through
    // `install_tool_default_extension`; they are not counted here.
    debug_assert_eq!(m.len(), 47, "tool defaults table must list all 47 tools");
    m
}

/// Does this row have NO claude-code counterpart?
///
/// True for `Workflow` (gated behind `WORKFLOW_SCRIPTS` upstream, absent from
/// external builds) and for every row a product installed. The oracle-parity
/// split in `tests::table_splits_into_the_parity_set_and_the_divergence`
/// keys on this, and so does the Plan-mode carve-out in
/// `policy_gate::read_only_default_auto_allows`: a divergence row that is not
/// plan-safe must not be auto-allowed while the user believes they are only
/// planning. Deliberately NOT extended to the oracle rows — several of them are
/// `AllowByDefault` without being plan-safe, and changing that would be a parity
/// change rather than a fix.
#[must_use]
pub fn is_divergence_tool(name: &str) -> bool {
    name == "Workflow" || extension().is_some_and(|ext| ext.rows.contains_key(name))
}

/// Look up the row for a tool name, distinguishing "no row" from "a row that
/// happens to say Deny". `tool_default` collapses both to `DenyByDefault`,
/// which makes a missing row indistinguishable from a deliberate deny at that
/// call site — callers that need to catch a missing row (e.g. a cross-crate
/// guard test) must use this instead.
#[must_use]
pub fn tool_default_row(name: &str) -> Option<PromptDefault> {
    TOOL_DEFAULTS
        .get_or_init(init_defaults)
        .get(name)
        .copied()
        .or_else(|| extension().and_then(|ext| ext.rows.get(name).copied()))
}

/// Every tool name that has a row in this table, sorted.
///
/// r2-tests-honesty-012: this is the REVERSE direction of
/// [`tool_default_row`]. That answers "does THIS tool have a row"; nothing
/// could answer "does this ROW name a tool that still exists", because
/// `TOOL_DEFAULTS` is a private `static` and only single-key lookups were
/// exported. Three guards constrain the BASE table's composition:
/// `init_defaults`'s own `debug_assert_eq!(m.len(), 47, …)`, the test
/// `table_splits_into_the_parity_set_and_the_divergence`'s `oracle == 46` /
/// `divergence == 1`, and the test
/// `the_counts_in_this_module_doc_are_the_counts_in_the_table`'s hand-bumped
/// bucket counts. None of those numbers moves for the orphan this function
/// exists for, because every one of them counts the base table alone: delete a
/// tool from a CONSUMER crate's table, leave its installed row, and they all
/// still hold. They also fail by naming a NUMBER rather than the orphaned row,
/// and they cannot see a consumer crate's tool table in any case, since
/// `permission` depends on none of them.
///
/// The guard for an installed extension therefore lives with its owner: it
/// compares this list with the owner's own tool table.
#[must_use]
pub fn tool_default_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = TOOL_DEFAULTS
        .get_or_init(init_defaults)
        .keys()
        .copied()
        .chain(extension().into_iter().flat_map(|ext| ext.rows.keys().copied()))
        .collect();
    names.sort_unstable();
    names
}

/// Look up the default Y/N decision for a tool name. Unknown tools → Deny.
#[must_use]
pub fn tool_default(name: &str) -> PromptDefault {
    tool_default_row(name).unwrap_or(PromptDefault::DenyByDefault)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_is_allow_by_default() {
        assert_eq!(tool_default("Read"), PromptDefault::AllowByDefault);
    }

    #[test]
    fn bash_is_deny_by_default() {
        assert_eq!(tool_default("Bash"), PromptDefault::DenyByDefault);
    }

    #[test]
    fn agent_is_allow_by_default() {
        assert_eq!(tool_default("Agent"), PromptDefault::AllowByDefault);
    }

    /// `Workflow` is the single hand-off of the create-app flow (and of any
    /// scripted multi-agent launch). `WorkflowTool::check_permissions` gates it
    /// below this row in EITHER of two mutually exclusive shapes — a
    /// `scriptPath` launch is re-asked as the canonical `Read` tool on the
    /// resolved file, an inline / named launch reads no caller-selected file and
    /// self-allows because the agents it spawns are individually permissioned —
    /// so a missing row here would double-prompt with no extra safety. A tool
    /// absent from this table falls through `tool_default` to the fail-closed
    /// `DenyByDefault`, unlike its siblings `Agent` and `Skill`.
    ///
    /// The Plan-mode consequence is covered by
    /// `policy_gate::plan_mode_divergence_test`.
    #[test]
    fn workflow_is_allow_by_default() {
        assert_eq!(tool_default("Workflow"), PromptDefault::AllowByDefault);
        assert!(
            is_divergence_tool("Workflow"),
            "Workflow is a divergence row, not oracle parity"
        );
    }

    #[test]
    fn write_edit_notebook_are_deny() {
        assert_eq!(tool_default("Write"), PromptDefault::DenyByDefault);
        assert_eq!(tool_default("Edit"), PromptDefault::DenyByDefault);
        assert_eq!(tool_default("NotebookEdit"), PromptDefault::DenyByDefault);
    }

    #[test]
    fn web_tools_are_deny() {
        assert_eq!(tool_default("WebFetch"), PromptDefault::DenyByDefault);
        assert_eq!(tool_default("WebSearch"), PromptDefault::DenyByDefault);
    }

    /// The MCP family splits exactly where the oracle's tool objects split on
    /// `checkPermissions`: the generic MCP call surface and the auth flow
    /// declare one (and are `passthrough`/`ask`-capable), while the three
    /// resource readers and the server-maintenance tools declare none and so
    /// take `At`'s `{behavior:"allow"}` default. Pinning both sides keeps a
    /// future re-audit from sliding the whole family one way.
    #[test]
    fn mcp_call_surface_is_deny_and_resource_reads_are_allow() {
        assert_eq!(tool_default("MCP"), PromptDefault::DenyByDefault);
        assert_eq!(tool_default("McpAuth"), PromptDefault::DenyByDefault);
        for name in [
            "ListMcpResourcesTool",
            "ReadMcpResourceTool",
            "ReadMcpResourceDirTool",
            "WaitForMcpServers",
        ] {
            assert_eq!(
                tool_default(name),
                PromptDefault::AllowByDefault,
                "{name} declares no checkPermissions upstream"
            );
        }
    }

    /// The rows the 2.1.270 re-audit moved. Every one of these tool objects
    /// declares no `checkPermissions` in the binary, so upstream never prompts
    /// for them on mode alone; this table used to, which is what made
    /// "create task" raise a permission request in every mode.
    #[test]
    fn tools_with_no_oracle_check_permissions_are_allow() {
        for name in [
            "TaskCreate",
            "TaskUpdate",
            "TaskStop",
            "TaskGet",
            "TaskList",
            "TaskOutput",
            "TodoWrite",
            "CronDelete",
            "CronList",
            "ExitWorktree",
            "ReportFindings",
            "PushNotification",
        ] {
            assert_eq!(
                tool_default(name),
                PromptDefault::AllowByDefault,
                "{name} declares no checkPermissions upstream"
            );
        }
        // The other half of the same scan: these DO declare one, so they keep
        // reaching the prompt. A blanket flip would have taken them too.
        for name in [
            "Bash",
            "Write",
            "Edit",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "SendMessage",
            "RemoteTrigger",
            "EnterWorktree",
            "CronCreate",
            "REPL",
            "PowerShell",
        ] {
            assert_eq!(
                tool_default(name),
                PromptDefault::DenyByDefault,
                "{name} declares checkPermissions upstream"
            );
        }
    }

    #[test]
    fn read_only_tools_are_allow() {
        assert_eq!(tool_default("Glob"), PromptDefault::AllowByDefault);
        assert_eq!(tool_default("Grep"), PromptDefault::AllowByDefault);
        assert_eq!(tool_default("LSP"), PromptDefault::AllowByDefault);
    }

    #[test]
    fn removed_team_tools_have_no_permission_rows() {
        let defaults = init_defaults();
        assert!(!defaults.contains_key("TeamCreate"));
        assert!(!defaults.contains_key("TeamDelete"));
    }

    #[test]
    fn unknown_tool_defaults_to_deny() {
        assert_eq!(tool_default("DoesNotExist"), PromptDefault::DenyByDefault);
        assert_eq!(tool_default(""), PromptDefault::DenyByDefault);
    }

    #[test]
    fn table_splits_into_the_parity_set_and_the_divergence() {
        let m = init_defaults();
        // Splitting the count is strictly stronger than asserting the total:
        // it catches BOTH a dropped oracle tool and a tool added without being
        // recorded as a divergence, which a single total hides. The base table
        // is counted directly: an installed extension must not move these.
        let oracle = m.keys().filter(|k| **k != "Workflow").count();
        let divergence = m.keys().filter(|k| **k == "Workflow").count();
        assert_eq!(oracle, 46, "oracle-parity tool count changed");
        assert_eq!(divergence, 1, "divergence row count changed");
        assert_eq!(m.len(), oracle + divergence);
        // `Workflow` must be booked as a divergence, never as oracle parity:
        // the M5-05 table has no row for it.
        assert!(is_divergence_tool("Workflow"));
        assert!(!is_divergence_tool("Agent"));
    }

    /// The module doc states the base counts. Assert each one so the
    /// documentation and table stay synchronized.
    #[test]
    fn the_counts_in_this_module_doc_are_the_counts_in_the_table() {
        let m = init_defaults();
        let count = |divergence: bool, want: PromptDefault| {
            m.iter()
                .filter(|(name, value)| (**name == "Workflow") == divergence && **value == want)
                .count()
        };
        assert_eq!(count(false, PromptDefault::DenyByDefault), 14, "oracle deny");
        assert_eq!(count(false, PromptDefault::AllowByDefault), 32, "oracle allow");
        assert_eq!(count(true, PromptDefault::AllowByDefault), 1, "divergence allow");
        assert_eq!(count(true, PromptDefault::DenyByDefault), 0, "divergence deny");
    }

    /// An installed extension adds rows and divergence status, nothing else.
    #[test]
    fn an_installed_extension_adds_rows_and_leaves_the_base_alone() {
        install_test_extension();
        assert_eq!(tool_default("ExtList"), PromptDefault::AllowByDefault);
        assert_eq!(tool_default("ExtMutate"), PromptDefault::DenyByDefault);
        assert_eq!(tool_default_row("ExtBuild"), Some(PromptDefault::AllowByDefault));
        assert_eq!(tool_default_row("ExtNope"), None);
        assert!(is_divergence_tool("ExtList"));
        assert!(!is_divergence_tool("ExtNope"));
        let names = tool_default_names();
        assert!(names.contains(&"ExtBuild") && names.contains(&"Read"));
        // The base table's own counts are untouched by the extension.
        assert_eq!(init_defaults().len(), 47);
        assert!(crate::mode_policy::is_plan_safe_tool("ExtList"));
        assert!(!crate::mode_policy::is_plan_safe_tool("ExtBuild"));
        assert_eq!(
            extension_rule_content("ExtMutate", &serde_json::json!({"id": "x"})).as_deref(),
            Some("x")
        );
        assert_eq!(extension_rule_content("Read", &serde_json::json!({"id": "x"})), None);
        assert_eq!(extension_rule_content("ExtMutate", &serde_json::json!({})), None);
    }
}

/// A small product of this crate's own, for tests of everything that reads the extension. It is
/// installed process-wide, like a real one, and every test that needs it calls this first.
///
/// `ExtList` is read-only and plan-safe, `ExtBuild` is an allow-by-default write (not plan-safe),
/// `ExtMutate` is a deny-by-default write whose rules are keyed by the call's `id`.
#[cfg(test)]
pub(crate) fn install_test_extension() {
    let mut rows = HashMap::new();
    rows.insert("ExtList", PromptDefault::AllowByDefault);
    rows.insert("ExtBuild", PromptDefault::AllowByDefault);
    rows.insert("ExtMutate", PromptDefault::DenyByDefault);
    let _ = install_tool_default_extension(ToolDefaultExtension {
        rows,
        plan_safe: &["ExtList"],
        rule_content: |name, input| {
            name.starts_with("Ext")
                .then(|| input.get("id")?.as_str().map(str::to_string))
                .flatten()
        },
    });
}
