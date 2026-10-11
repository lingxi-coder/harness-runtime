//! The two tool allowlists that let a call skip a prompt on mode alone.
//!
//! * [`AUTO_MODE_SAFE_TOOLS`] is the AUTO-mode set — the 2.1.270 `ojo` set read
//!   by `Sft(e,n)`. A tool on it is allowed outright in Auto mode and never
//!   reaches the classifier.
//! * [`PLAN_SAFE_TOOLS`] is the PLAN-mode mutation-block allowlist: in `Plan`
//!   mode every tool NOT on this list is treated as a potential state mutation
//!   and asked about (see [`crate::policy::PermissionPolicy::authorize`]).
//!
//! They started as one list (the port reused the EXTERNAL subset of
//! `SAFE_YOLO_ALLOWLISTED_TOOLS` for both) and have since diverged at the
//! source: 2.1.270's `ojo` dropped `SendMessage` / `AskUserQuestion` and grew
//! the MCP-maintenance rows, while the Plan backstop is this port's own
//! construct with no upstream twin (upstream enforces Plan mode by not
//! ADVERTISING mutating tools, so a plan-mode `AskUserQuestion` must keep
//! working). Keeping them separate is what lets each track its own source.
//!
//! ## Divergence from TS
//! - The TS set is built from per-tool `*_TOOL_NAME` constants imported across
//!   the tool modules. This crate has no dependency on the tools crate (and
//!   must not add one), so the wire names are inlined as string literals that
//!   match the names this port advertises (see
//!   [`crate::defaults_per_tool`]). These now match the TS constant values
//!   byte-for-byte, including the MCP resource tools' `Tool` suffix
//!   (`ListMcpResourcesTool` / `ReadMcpResourceTool`, per
//!   `classifierDecision.ts:56-98`).
//! - The `ant`-only safe tools (`TerminalCapture`, `OverflowTest`,
//!   `VerifyPlanExecution`) and the internal `YoloClassifier` tool are OMITTED
//!   — they are gated behind `USER_TYPE==='ant'` / feature flags in TS and are
//!   absent from external builds, so the external allowlist is byte-faithful
//!   without them.
//! - `Workflow` is conditional on the `WORKFLOW_SCRIPTS` feature in TS and is
//!   not part of this port's tool set, so it is omitted.

/// External read-only / planning-safe allowlist (the `Plan`-mode pass set).
///
/// Mirrors `SAFE_YOLO_ALLOWLISTED_TOOLS` / 2.1.211 `FLg`
/// (`classifierDecision.ts:56-94`), external subset. A tool on this list never
/// trips the Plan-mode mutation backstop; everything else does. Read-only
/// file/search operations, MCP resource reads, task/todo metadata, plan-mode
/// UI tools, and the `SendMessage` mailbox tool are all safe. `TeamCreate`/
/// `TeamDelete`/`Sleep` were REMOVED in 2.1.211 (MODE-SAFE-LIST-06) and are no
/// longer plan-safe.
const PLAN_SAFE_TOOLS: &[&str] = &[
    // Read-only file operations.
    "Read",
    // Search / read-only.
    "Grep",
    "Glob",
    "LSP",
    "ToolSearch",
    "ListMcpResourcesTool",
    "ReadMcpResourceTool",
    // Task management (metadata only).
    "TodoWrite",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "TaskStop",
    "TaskOutput",
    // Plan mode / UI.
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    // Swarm coordination (internal mailbox only — teammates run their own
    // permission checks, so no actual security bypass). MODE-SAFE-LIST-06:
    // 2.1.211's isAutoModeAllowlistedTool set (FLg) keeps `SendMessage` (uf)
    // but NO LONGER lists `TeamCreate`/`TeamDelete` (those now appear only in
    // the legacy-names set JUr, not the plan-safe allowlist) and has no `Sleep`
    // tool-name string at all — so all three are removed here and now trip the
    // Plan-mode mutation backstop like any other unlisted tool.
    "SendMessage",
];

/// The 2.1.270 AUTO-MODE safe allowlist — 1:1 with the binary's `ojo` set, read
/// by `Sft(e,n)` (src_169588164.js @2150529) and consumed by the auto-mode
/// decision `dKo` as:
///
/// ```js
/// if(Xn===void 0&&Pe===void 0&&!Le&&Sft(e.name,n)){
///   …t(`Skipping auto mode classifier for ${e.name}: tool is on the safe allowlist`),
///   …De({updatedInput:M.updatedInput??n,decisionReason:{type:"mode",mode:"auto"}})}
/// ```
///
/// i.e. a tool on this list is ALLOWED in Auto mode without paying a classifier
/// round-trip. This is a DIFFERENT list from [`PLAN_SAFE_TOOLS`] even though the
/// port originally reused one set for both: by 2.1.270 `ojo` no longer carries
/// `SendMessage` or `AskUserQuestion` (both have their own `checkPermissions`
/// and, for `AskUserQuestion`, `requiresUserInteraction`), while it has grown
/// the MCP-maintenance and task-metadata rows below. The Plan-mode backstop is
/// this port's own construct (upstream enforces Plan mode by not ADVERTISING
/// mutating tools), so the two sets are kept separate rather than merged.
///
/// Rows with no counterpart in this port are listed anyway so the set stays
/// byte-comparable against the binary; they are simply never queried. The
/// binary additionally splices `PLUGIN_SKILL_SAFE_TOOL_NAMES` (six plugin-skill
/// tools: `SuggestSkills`, `SuggestPluginInstall`, …) and the Chrome/browser MCP
/// prefix families handled by `Sft`'s `sjo`/`BPn`/`UPn`/`HPn` arms — all of
/// which are per-INPUT predicates over tools this port does not advertise, so
/// they are documented omissions rather than rows.
const AUTO_MODE_SAFE_TOOLS: &[&str] = &[
    // Read-only file + search (`rt`, `co`, `bo`, `MW`, `ji`).
    "Read",
    "Grep",
    "Glob",
    "LSP",
    "ToolSearch",
    // MCP resource reads and server maintenance (`X5`, `AW`, `vW`, `WM`, `eD`).
    "ListMcpResourcesTool",
    "ReadMcpResourceTool",
    "ReadMcpResourceDirTool",
    "RefreshMcpTools",
    "WaitForMcpServers",
    // Review reporting (`t0`).
    "ReportFindings",
    // Task / todo metadata (`Ay`, `uw`, `VF`, `pw`, `uS`, `Kg`, `A2`, `dw`).
    "TodoWrite",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "TaskStop",
    "TaskOutput",
    "GetTask",
    // Plan-mode UI (`TC`, `Dy`).
    "EnterPlanMode",
    "ExitPlanMode",
    // Onboarding / connector discovery (`I0e`, `uJe`, `PPn`, `IPn`, `MPn`).
    // Present in `ojo`; this port advertises none of them.
    "ConnectGitHub",
    "ShowOnboardingRolePicker",
    "SearchMcpRegistry",
    "SuggestConnectors",
    "ListConnectors",
];

/// Is `tool_name` on the auto-mode safe allowlist (`Sft` / `ojo`)?
///
/// `true` ⇒ Auto mode allows the call outright, with
/// `decisionReason: {type:"mode", mode:"auto"}`, and never reaches the
/// classifier. `false` ⇒ the call continues down `dKo` to the classifier.
#[must_use]
pub fn is_auto_mode_safe_tool(tool_name: &str) -> bool {
    AUTO_MODE_SAFE_TOOLS.contains(&tool_name)
}

/// Is `tool_name` on the read-only / planning-safe allowlist?
///
/// Used by the Plan-mode backstop in [`crate::policy::PermissionPolicy::authorize`]:
/// a tool that is NOT plan-safe is treated as a state mutation and asked about
/// when no allow rule already matched. Returns `true` for the read-only /
/// planning-safe tools enumerated in [`PLAN_SAFE_TOOLS`] or installed by a
/// product (`ToolDefaultExtension::plan_safe`), `false` otherwise
/// (mutating tools like `Edit`/`Write`/`Bash`, and unknown tools, fail closed).
#[must_use]
pub fn is_plan_safe_tool(tool_name: &str) -> bool {
    PLAN_SAFE_TOOLS.contains(&tool_name)
        || crate::defaults_per_tool::extension()
            .is_some_and(|ext| ext.plan_safe.contains(&tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_mode_safe_set_matches_270_ojo() {
        // Every row the binary's `ojo` carries that this port can advertise.
        for t in [
            "Read",
            "Grep",
            "Glob",
            "LSP",
            "ToolSearch",
            "ListMcpResourcesTool",
            "ReadMcpResourceTool",
            "ReadMcpResourceDirTool",
            "RefreshMcpTools",
            "WaitForMcpServers",
            "ReportFindings",
            "TodoWrite",
            "TaskCreate",
            "TaskGet",
            "TaskUpdate",
            "TaskList",
            "TaskStop",
            "TaskOutput",
            "GetTask",
            "EnterPlanMode",
            "ExitPlanMode",
            "ConnectGitHub",
            "ShowOnboardingRolePicker",
            "SearchMcpRegistry",
            "SuggestConnectors",
            "ListConnectors",
        ] {
            assert!(is_auto_mode_safe_tool(t), "{t} is in 2.1.270 `ojo`");
        }
        assert_eq!(AUTO_MODE_SAFE_TOOLS.len(), 26);
    }

    #[test]
    fn auto_mode_safe_set_excludes_what_270_ojo_dropped() {
        // `Sft` is NOT the Plan list. `AskUserQuestion` carries
        // `requiresUserInteraction()` and `SendMessage` its own
        // `checkPermissions`; neither is in `ojo`, so both must still reach the
        // auto-mode classifier rather than be waved through.
        for t in ["AskUserQuestion", "SendMessage"] {
            assert!(!is_auto_mode_safe_tool(t), "{t} is not in 2.1.270 `ojo`");
            assert!(is_plan_safe_tool(t), "{t} stays plan-safe");
        }
        // Side-effecting tools are on neither list.
        for t in ["Bash", "Edit", "Write", "WebFetch", "Agent", "CronCreate"] {
            assert!(!is_auto_mode_safe_tool(t), "{t} must reach the classifier");
        }
    }

    #[test]
    fn read_only_and_search_tools_are_plan_safe() {
        for t in ["Read", "Grep", "Glob", "LSP", "ToolSearch"] {
            assert!(is_plan_safe_tool(t), "{t} should be plan-safe");
        }
    }

    #[test]
    fn mcp_resource_reads_are_plan_safe() {
        assert!(is_plan_safe_tool("ListMcpResourcesTool"));
        assert!(is_plan_safe_tool("ReadMcpResourceTool"));
    }

    #[test]
    fn task_and_todo_metadata_tools_are_plan_safe() {
        for t in [
            "TodoWrite",
            "TaskCreate",
            "TaskGet",
            "TaskUpdate",
            "TaskList",
            "TaskStop",
            "TaskOutput",
        ] {
            assert!(is_plan_safe_tool(t), "{t} should be plan-safe");
        }
    }

    #[test]
    fn plan_ui_tools_are_plan_safe() {
        assert!(is_plan_safe_tool("AskUserQuestion"));
        assert!(is_plan_safe_tool("EnterPlanMode"));
        assert!(is_plan_safe_tool("ExitPlanMode"));
    }

    #[test]
    fn send_message_is_plan_safe() {
        // 2.1.211 FLg still lists `SendMessage` (uf).
        assert!(is_plan_safe_tool("SendMessage"));
    }

    #[test]
    fn team_and_sleep_tools_are_not_plan_safe() {
        // MODE-SAFE-LIST-06: 2.1.211 dropped `TeamCreate`/`TeamDelete` from the
        // plan-safe allowlist (they now live only in the legacy-names set JUr)
        // and removed the `Sleep` tool-name entirely. They must now trip the
        // Plan-mode mutation backstop.
        assert!(!is_plan_safe_tool("TeamCreate"));
        assert!(!is_plan_safe_tool("TeamDelete"));
        assert!(!is_plan_safe_tool("Sleep"));
    }

    #[test]
    fn mutating_tools_are_not_plan_safe() {
        // Editors, shell, and external side-effect tools must trip the block.
        for t in [
            "Edit",
            "Write",
            "NotebookEdit",
            "Bash",
            "PowerShell",
            "WebFetch",
            "WebSearch",
        ] {
            assert!(!is_plan_safe_tool(t), "{t} must NOT be plan-safe");
        }
    }

    #[test]
    fn ant_only_and_classifier_tools_are_omitted() {
        // External builds never advertise these — they must not be plan-safe.
        for t in [
            "TerminalCapture",
            "OverflowTest",
            "VerifyPlanExecution",
            "YoloClassifier",
            "Workflow",
        ] {
            assert!(!is_plan_safe_tool(t), "{t} is ant/feature-gated, omitted");
        }
    }

    #[test]
    fn unknown_tool_is_not_plan_safe() {
        assert!(!is_plan_safe_tool("DoesNotExist"));
        assert!(!is_plan_safe_tool(""));
    }
}
