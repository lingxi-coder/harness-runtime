//! `tengu_tool_*` event schemas — 40 events emitted by the tool dispatcher,
//! permission gate, and per-tool implementations (M4 owner; M3-06 schema lock).
//!
//! Spec §7 line 768-779. Covers the generic tool lifecycle (started/completed/
//! failed/cancelled), the permission gate, and per-tool started/completed/
//! failed triads for bash, edit, read, write, `web_fetch`, task,
//! notebook, and MCP, plus a single skill-invocation event. Grep/Glob emit
//! NO telemetry (claude-code v2.1.183 emits no `tengu_tool_grep_*` /
//! `tengu_tool_glob_*` events; the port matches it). User-derived
//! string identifiers are typed [`Verified`](crate::Verified); free-form user
//! content that may carry filepaths, secrets, or org-internal hostnames is
//! [`PiiTagged`](crate::PiiTagged).

use crate::pii::{PiiTagged, Verified};
use serde::{Deserialize, Serialize};

// -- Event name constants (byte-locked) ---------------------------------------

/// `tengu_tool_started` — generic per-invocation start (before dispatch).
pub const STARTED: &str = "tengu_tool_started";
/// `tengu_tool_completed` — generic per-invocation success.
pub const COMPLETED: &str = "tengu_tool_completed";
/// `tengu_tool_failed` — generic per-invocation failure.
pub const FAILED: &str = "tengu_tool_failed";
/// `tengu_tool_cancelled` — caller cancelled an in-flight tool call.
pub const CANCELLED: &str = "tengu_tool_cancelled";
/// `tengu_tool_permission_requested` — permission prompt presented to the user.
pub const PERMISSION_REQUESTED: &str = "tengu_tool_permission_requested";
/// `tengu_tool_permission_granted` — user (or remembered rule) allowed the action.
pub const PERMISSION_GRANTED: &str = "tengu_tool_permission_granted";
/// `tengu_tool_permission_denied` — user (or remembered rule) refused the action.
pub const PERMISSION_DENIED: &str = "tengu_tool_permission_denied";
/// `tengu_tool_permission_remembered` — decision was persisted for future calls.
pub const PERMISSION_REMEMBERED: &str = "tengu_tool_permission_remembered";
/// `tengu_tool_bash_started` — bash invocation about to spawn.
pub const BASH_STARTED: &str = "tengu_tool_bash_started";
/// `tengu_tool_bash_completed` — bash invocation exited.
pub const BASH_COMPLETED: &str = "tengu_tool_bash_completed";
/// `tengu_tool_bash_failed` — bash invocation errored before producing an exit code.
pub const BASH_FAILED: &str = "tengu_tool_bash_failed";
/// `tengu_tool_bash_timeout` — bash invocation hit the timeout watchdog.
///
/// This `tengu_tool_bash_*` family is a LingXi naming scheme, not a rename of
/// anything upstream: a grep of the 2.1.263 sources for `tengu_tool_bash`
/// returns zero (while other `tengu_*` needles from the same file match), so
/// there is no oracle event these drifted from. The three constants BELOW are
/// the oracle's own backgrounding events, which the port was missing entirely.
pub const BASH_TIMEOUT: &str = "tengu_tool_bash_timeout";

/// `tengu_bash_command_explicitly_backgrounded` — the model asked for
/// `run_in_background: true` and the command was handed off
/// (claude-code `src_160988549.js` @4343453,
/// `i("tengu_bash_command_explicitly_backgrounded",{command_type:Npe(ve)})`).
pub const BASH_EXPLICITLY_BACKGROUNDED: &str = "tengu_bash_command_explicitly_backgrounded";

/// `tengu_bash_command_timeout_backgrounded` — the command outlived its timeout
/// and was moved to the background instead of killed
/// (`wn("tengu_bash_command_timeout_backgrounded",hn)`, @4343287).
pub const BASH_TIMEOUT_BACKGROUNDED: &str = "tengu_bash_command_timeout_backgrounded";

/// `tengu_bash_command_turn_abort_backgrounded` — a turn abort moved a running
/// command to the background rather than killing it
/// (`wn("tengu_bash_command_turn_abort_backgrounded")`, @4344733).
///
/// Emitted by the Bash runner after a cancelled foreground command has
/// actually detached; ordinary timeout/manual transitions do not emit it.
pub const BASH_TURN_ABORT_BACKGROUNDED: &str = "tengu_bash_command_turn_abort_backgrounded";
/// `tengu_tool_edit_started` — Edit tool began applying a replacement.
pub const EDIT_STARTED: &str = "tengu_tool_edit_started";
/// `tengu_tool_edit_completed` — Edit tool wrote the modified file.
pub const EDIT_COMPLETED: &str = "tengu_tool_edit_completed";
/// `tengu_tool_edit_failed` — Edit tool errored (mismatch, IO, etc.).
pub const EDIT_FAILED: &str = "tengu_tool_edit_failed";
/// `tengu_tool_read_started` — Read tool began reading a file.
pub const READ_STARTED: &str = "tengu_tool_read_started";
/// `tengu_tool_read_completed` — Read tool returned file contents.
pub const READ_COMPLETED: &str = "tengu_tool_read_completed";
/// `tengu_tool_read_failed` — Read tool errored.
pub const READ_FAILED: &str = "tengu_tool_read_failed";
/// `tengu_tool_write_started` — Write tool began writing a file.
pub const WRITE_STARTED: &str = "tengu_tool_write_started";
/// `tengu_tool_write_completed` — Write tool finished writing a file.
pub const WRITE_COMPLETED: &str = "tengu_tool_write_completed";
/// `tengu_tool_write_failed` — Write tool errored.
pub const WRITE_FAILED: &str = "tengu_tool_write_failed";
/// `tengu_subagent_md_report_blocked` — a subagent's Write of a
/// `REPORT`/`SUMMARY`/`FINDINGS`/`ANALYSIS` `*.md` report file was hard-blocked
/// (subagents must return findings as text). Payload carries `contentBytes`.
pub const SUBAGENT_MD_REPORT_BLOCKED: &str = "tengu_subagent_md_report_blocked";
/// `tengu_repair_double_escaped_unicode` — a model-emitted tool_use input
/// contained literal `\uXXXX` TEXT that was rewritten into real characters
/// (cc 2.1.218 `jYd`). Fields: `repaired_strings`, `windows_path_skips`
/// (strings left verbatim because they look like a Windows path). Emitted once
/// per tool_use, only when a counter is non-zero.
pub const REPAIR_DOUBLE_ESCAPED_UNICODE: &str = "tengu_repair_double_escaped_unicode";
/// `tengu_tool_web_fetch_started` — `WebFetch` tool began an HTTP request.
pub const WEB_FETCH_STARTED: &str = "tengu_tool_web_fetch_started";
/// `tengu_tool_web_fetch_completed` — `WebFetch` tool received the response.
pub const WEB_FETCH_COMPLETED: &str = "tengu_tool_web_fetch_completed";
/// `tengu_tool_web_fetch_failed` — `WebFetch` tool errored (network, status code).
pub const WEB_FETCH_FAILED: &str = "tengu_tool_web_fetch_failed";
/// `tengu_tool_web_search_started` — `WebSearch` tool dispatched a query (M4-03).
pub const WEB_SEARCH_STARTED: &str = "tengu_tool_web_search_started";
/// `tengu_tool_web_search_completed` — `WebSearch` tool returned search results (M4-03).
pub const WEB_SEARCH_COMPLETED: &str = "tengu_tool_web_search_completed";
/// `tengu_tool_web_search_failed` — `WebSearch` tool errored (HTTP, parse, etc.) (M4-03).
pub const WEB_SEARCH_FAILED: &str = "tengu_tool_web_search_failed";
/// `tengu_tool_task_dispatched` — Agent tool spawned a subagent.
pub const TASK_DISPATCHED: &str = "tengu_tool_task_dispatched";
/// `tengu_tool_task_completed` — Agent subagent returned successfully.
pub const TASK_COMPLETED: &str = "tengu_tool_task_completed";
/// `tengu_tool_task_failed` — Agent subagent errored.
pub const TASK_FAILED: &str = "tengu_tool_task_failed";
/// `tengu_tool_notebook_started` — `NotebookEdit` tool began editing a notebook.
pub const NOTEBOOK_STARTED: &str = "tengu_tool_notebook_started";
/// `tengu_tool_notebook_completed` — `NotebookEdit` tool wrote the modified notebook.
pub const NOTEBOOK_COMPLETED: &str = "tengu_tool_notebook_completed";
/// `tengu_tool_notebook_failed` — `NotebookEdit` tool errored.
pub const NOTEBOOK_FAILED: &str = "tengu_tool_notebook_failed";
/// `tengu_tool_mcp_invoked` — MCP tool call was dispatched to a server.
pub const MCP_INVOKED: &str = "tengu_tool_mcp_invoked";
/// `tengu_tool_mcp_completed` — MCP tool call returned a result.
pub const MCP_COMPLETED: &str = "tengu_tool_mcp_completed";
/// `tengu_tool_mcp_failed` — MCP tool call errored.
pub const MCP_FAILED: &str = "tengu_tool_mcp_failed";
/// `tengu_tool_skill_invoked` — Skill tool ran a registered skill.
pub const SKILL_INVOKED: &str = "tengu_tool_skill_invoked";
/// `tengu_tool_powershell_started` — `PowerShell` invocation about to spawn (M4-02).
pub const POWERSHELL_STARTED: &str = "tengu_tool_powershell_started";
/// `tengu_tool_powershell_completed` — `PowerShell` invocation exited (M4-02).
pub const POWERSHELL_COMPLETED: &str = "tengu_tool_powershell_completed";
/// `tengu_tool_powershell_failed` — `PowerShell` invocation errored (M4-02).
pub const POWERSHELL_FAILED: &str = "tengu_tool_powershell_failed";
/// `tengu_tool_repl_started` — `REPL` snippet about to execute (M4-02).
pub const REPL_STARTED: &str = "tengu_tool_repl_started";
/// `tengu_tool_repl_completed` — `REPL` snippet finished (M4-02).
pub const REPL_COMPLETED: &str = "tengu_tool_repl_completed";
/// `tengu_tool_repl_failed` — `REPL` snippet errored (M4-02).
pub const REPL_FAILED: &str = "tengu_tool_repl_failed";
/// `tengu_tool_sleep_started` — `Sleep` invocation began (M4-02).
pub const SLEEP_STARTED: &str = "tengu_tool_sleep_started";
/// `tengu_tool_sleep_completed` — `Sleep` invocation woke (M4-02).
pub const SLEEP_COMPLETED: &str = "tengu_tool_sleep_completed";
/// `tengu_tool_sleep_failed` — `Sleep` invocation rejected/errored (M4-02).
pub const SLEEP_FAILED: &str = "tengu_tool_sleep_failed";

// -- M4-04 Workflow tools ------------------------------------------------------
//
// NOTE: claude's TodoWrite emits NO per-tool telemetry (per-call success/error
// is recorded by the generic tool dispatcher), so the port no longer defines or
// fires tengu_tool_todo_write_started/completed/failed — removed for strict
// 2.1.195 parity (0 hits in the binary).

/// `tengu_tool_enter_plan_mode_started` — `EnterPlanModeTool` began (M4-04).
pub const ENTER_PLAN_MODE_STARTED: &str = "tengu_tool_enter_plan_mode_started";
/// `tengu_tool_enter_plan_mode_completed` — `EnterPlanModeTool` flipped the flag (M4-04).
pub const ENTER_PLAN_MODE_COMPLETED: &str = "tengu_tool_enter_plan_mode_completed";
/// `tengu_tool_enter_plan_mode_failed` — `EnterPlanModeTool` errored (already in mode) (M4-04).
pub const ENTER_PLAN_MODE_FAILED: &str = "tengu_tool_enter_plan_mode_failed";

/// `tengu_tool_exit_plan_mode_started` — `ExitPlanModeTool` began (M4-04).
pub const EXIT_PLAN_MODE_STARTED: &str = "tengu_tool_exit_plan_mode_started";
/// `tengu_tool_exit_plan_mode_completed` — `ExitPlanModeTool` flipped the flag (M4-04).
pub const EXIT_PLAN_MODE_COMPLETED: &str = "tengu_tool_exit_plan_mode_completed";
/// `tengu_tool_exit_plan_mode_failed` — `ExitPlanModeTool` errored (not in mode) (M4-04).
pub const EXIT_PLAN_MODE_FAILED: &str = "tengu_tool_exit_plan_mode_failed";

/// `tengu_tool_enter_worktree_started` — `EnterWorktreeTool` began (M4-04).
pub const ENTER_WORKTREE_STARTED: &str = "tengu_tool_enter_worktree_started";
/// `tengu_tool_enter_worktree_completed` — `EnterWorktreeTool` created a worktree (M4-04).
pub const ENTER_WORKTREE_COMPLETED: &str = "tengu_tool_enter_worktree_completed";
/// `tengu_tool_enter_worktree_failed` — `EnterWorktreeTool` errored (bad slug, git, IO) (M4-04).
pub const ENTER_WORKTREE_FAILED: &str = "tengu_tool_enter_worktree_failed";

/// `tengu_worktree_created` — 2.1.206 `EnterWorktree` byte-exact success event
/// fired when a NEW worktree was created (`SCd.call`'s
/// `N(e.path?"tengu_worktree_entered_existing":"tengu_worktree_created",{mid_session:true})`,
/// the `!e.path` branch). Distinct from the M4-04 lifecycle triad above
/// (`ENTER_WORKTREE_STARTED`/`COMPLETED`/`FAILED`), which the port keeps for its
/// own started/completed/failed bookkeeping; this is the extra byte-exact
/// single event the 206 oracle actually emits, fired ADDITIONALLY on success.
pub const WORKTREE_CREATED: &str = "tengu_worktree_created";
/// `tengu_worktree_entered_existing` — 2.1.206 `EnterWorktree` byte-exact
/// success event fired when switching into an ALREADY-EXISTING worktree (the
/// `e.path` branch, both the pinned-cwd/subagent-isolation path and the normal
/// mid-session path in `SCd.call`).
pub const WORKTREE_ENTERED_EXISTING: &str = "tengu_worktree_entered_existing";

/// `tengu_tool_exit_worktree_started` — `ExitWorktreeTool` began (M4-04).
pub const EXIT_WORKTREE_STARTED: &str = "tengu_tool_exit_worktree_started";
/// `tengu_tool_exit_worktree_completed` — `ExitWorktreeTool` removed a worktree (M4-04).
pub const EXIT_WORKTREE_COMPLETED: &str = "tengu_tool_exit_worktree_completed";
/// `tengu_tool_exit_worktree_failed` — `ExitWorktreeTool` errored (M4-04).
pub const EXIT_WORKTREE_FAILED: &str = "tengu_tool_exit_worktree_failed";

/// `tengu_worktree_kept` — 2.1.206 `ExitWorktree` byte-exact success event
/// fired when `action:"keep"` succeeded (the worktree + branch are left on
/// disk). Distinct from the M4-04 lifecycle triad above
/// (`EXIT_WORKTREE_STARTED`/`COMPLETED`/`FAILED`), which the port keeps for
/// its own started/completed/failed bookkeeping; this is the extra
/// byte-exact single event the 206 oracle actually emits, fired ADDITIONALLY
/// on success.
pub const WORKTREE_KEPT: &str = "tengu_worktree_kept";
/// `tengu_worktree_removed` — 2.1.206 `ExitWorktree` byte-exact success event
/// fired when `action:"remove"` succeeded (the worktree + branch were
/// deleted).
pub const WORKTREE_REMOVED: &str = "tengu_worktree_removed";

// ===== M4-05 Agent + Task tools (24 events, 8 tools × 3 lifecycle stages) =====

/// `tengu_tool_agent_started` — `AgentTool` began spawning a subagent (M4-05).
pub const AGENT_STARTED: &str = "tengu_tool_agent_started";
/// `tengu_tool_agent_completed` — `AgentTool` subagent returned Completed (M4-05).
///
/// Distinct from the legacy `TASK_COMPLETED = "tengu_tool_task_completed"` event
/// (M3-06 baseline, kept for back-compat); the M4-05 const is suffixed with
/// `_M4_05` to avoid identifier collision with the legacy constant.
pub const AGENT_COMPLETED_M4_05: &str = "tengu_tool_agent_completed";
/// `tengu_tool_agent_failed` — `AgentTool` subagent errored or was killed (M4-05).
pub const AGENT_FAILED: &str = "tengu_tool_agent_failed";

/// `tengu_tool_task_create_started` — `TaskCreateTool` validated input (M4-05).
pub const TASK_CREATE_STARTED: &str = "tengu_tool_task_create_started";
/// `tengu_tool_task_create_completed` — `TaskCreateTool` allocated spool + id (M4-05).
pub const TASK_CREATE_COMPLETED: &str = "tengu_tool_task_create_completed";
/// `tengu_tool_task_create_failed` — `TaskCreateTool` errored (M4-05).
pub const TASK_CREATE_FAILED: &str = "tengu_tool_task_create_failed";

/// `tengu_tool_task_get_started` — `TaskGetTool` accepted the request (M4-05).
pub const TASK_GET_STARTED: &str = "tengu_tool_task_get_started";
/// `tengu_tool_task_get_completed` — `TaskGetTool` returned a state row (M4-05).
pub const TASK_GET_COMPLETED: &str = "tengu_tool_task_get_completed";
/// `tengu_tool_task_get_failed` — `TaskGetTool` errored (M4-05).
pub const TASK_GET_FAILED: &str = "tengu_tool_task_get_failed";

/// `tengu_tool_task_list_started` — `TaskListTool` began enumeration (M4-05).
pub const TASK_LIST_STARTED: &str = "tengu_tool_task_list_started";
/// `tengu_tool_task_list_completed` — `TaskListTool` returned the snapshot (M4-05).
pub const TASK_LIST_COMPLETED: &str = "tengu_tool_task_list_completed";
/// `tengu_tool_task_list_failed` — `TaskListTool` errored (M4-05).
pub const TASK_LIST_FAILED: &str = "tengu_tool_task_list_failed";

/// `tengu_tool_task_update_started` — `TaskUpdateTool` validated input (M4-05).
pub const TASK_UPDATE_STARTED: &str = "tengu_tool_task_update_started";
/// `tengu_tool_task_update_completed` — `TaskUpdateTool` mutated status (M4-05).
pub const TASK_UPDATE_COMPLETED: &str = "tengu_tool_task_update_completed";
/// `tengu_tool_task_update_failed` — `TaskUpdateTool` errored (M4-05).
pub const TASK_UPDATE_FAILED: &str = "tengu_tool_task_update_failed";

/// `tengu_tool_task_stop_started` — `TaskStopTool` validated input (M4-05).
pub const TASK_STOP_STARTED: &str = "tengu_tool_task_stop_started";
/// `tengu_tool_task_stop_completed` — `TaskStopTool` killed the task (M4-05).
pub const TASK_STOP_COMPLETED: &str = "tengu_tool_task_stop_completed";
/// `tengu_tool_task_stop_failed` — `TaskStopTool` errored (M4-05).
pub const TASK_STOP_FAILED: &str = "tengu_tool_task_stop_failed";

/// `tengu_tool_task_output_started` — `TaskOutputTool` began spool read (M4-05).
pub const TASK_OUTPUT_STARTED: &str = "tengu_tool_task_output_started";
/// `tengu_tool_task_output_completed` — `TaskOutputTool` returned content (M4-05).
pub const TASK_OUTPUT_COMPLETED: &str = "tengu_tool_task_output_completed";
/// `tengu_tool_task_output_failed` — `TaskOutputTool` errored (M4-05).
pub const TASK_OUTPUT_FAILED: &str = "tengu_tool_task_output_failed";

/// `tengu_tool_send_message_started` — `SendMessageTool` validated input (M4-05).
pub const SEND_MESSAGE_STARTED: &str = "tengu_tool_send_message_started";
/// `tengu_tool_send_message_completed` — `SendMessageTool` enqueued (M4-05).
pub const SEND_MESSAGE_COMPLETED: &str = "tengu_tool_send_message_completed";
/// `tengu_tool_send_message_failed` — `SendMessageTool` errored (M4-05).
pub const SEND_MESSAGE_FAILED: &str = "tengu_tool_send_message_failed";

// ===== M4-07 MCP + LSP tools (13 new events) ================================
//
// Note: `MCP_COMPLETED` and `MCP_FAILED` are already declared above (M3-06
// baseline at lines 96-100); we reuse them and only add `MCP_STARTED`
// here so the wire-string set grows by 13 (not 15 — deviation from plan
// header: legacy `MCP_INVOKED` was M3-06's "started" surrogate, but the
// M4-06 baseline lacked the `tengu_tool_mcp_started` literal that M4-07
// requires).

/// `tengu_tool_mcp_started` — `MCPTool` (M4-07 dispatcher) began execution.
pub const MCP_STARTED: &str = "tengu_tool_mcp_started";

/// `tengu_mcp_tool_auto_backgrounded` — a long-running MCP `tools/call`
/// exceeded `getMcpAutoBackgroundMs` and was moved to the background as an
/// `mcp_task` (claude-code 2.1.212 `callMcpToolWithAutoBackground`/`Gc_`). Note
/// the wire name is bare `tengu_mcp_tool_auto_backgrounded` — it does NOT carry
/// the `tengu_tool_` prefix of the sibling MCP dispatcher events.
pub const MCP_TOOL_AUTO_BACKGROUNDED: &str = "tengu_mcp_tool_auto_backgrounded";

/// `tengu_tool_mcp_auth_started` — `McpAuthTool` began inspection (M4-07).
pub const MCP_AUTH_STARTED: &str = "tengu_tool_mcp_auth_started";
/// `tengu_tool_mcp_auth_completed` — `McpAuthTool` finished (M4-07).
pub const MCP_AUTH_COMPLETED: &str = "tengu_tool_mcp_auth_completed";
/// `tengu_tool_mcp_auth_failed` — `McpAuthTool` errored (M4-07).
pub const MCP_AUTH_FAILED: &str = "tengu_tool_mcp_auth_failed";

/// `tengu_tool_list_mcp_resources_started` — `ListMcpResourcesTool` began (M4-07).
pub const LIST_MCP_RESOURCES_STARTED: &str = "tengu_tool_list_mcp_resources_started";
/// `tengu_tool_list_mcp_resources_completed` — `ListMcpResourcesTool` finished (M4-07).
pub const LIST_MCP_RESOURCES_COMPLETED: &str = "tengu_tool_list_mcp_resources_completed";
/// `tengu_tool_list_mcp_resources_failed` — `ListMcpResourcesTool` errored (M4-07).
pub const LIST_MCP_RESOURCES_FAILED: &str = "tengu_tool_list_mcp_resources_failed";

/// `tengu_tool_read_mcp_resource_started` — `ReadMcpResourceTool` began (M4-07).
pub const READ_MCP_RESOURCE_STARTED: &str = "tengu_tool_read_mcp_resource_started";
/// `tengu_tool_read_mcp_resource_completed` — `ReadMcpResourceTool` finished (M4-07).
pub const READ_MCP_RESOURCE_COMPLETED: &str = "tengu_tool_read_mcp_resource_completed";
/// `tengu_tool_read_mcp_resource_failed` — `ReadMcpResourceTool` errored (M4-07).
pub const READ_MCP_RESOURCE_FAILED: &str = "tengu_tool_read_mcp_resource_failed";

/// `tengu_tool_lsp_started` — `LSPTool` began (M4-07).
/// `tengu_tool_ask_user_question_started` — `AskUserQuestion` presented options to user (M4-08).
pub const ASK_USER_QUESTION_STARTED: &str = "tengu_tool_ask_user_question_started";
/// `tengu_tool_ask_user_question_completed` — user selected an option (M4-08).
pub const ASK_USER_QUESTION_COMPLETED: &str = "tengu_tool_ask_user_question_completed";
/// `tengu_tool_ask_user_question_failed` — resolver errored or rejected input (M4-08).
pub const ASK_USER_QUESTION_FAILED: &str = "tengu_tool_ask_user_question_failed";
/// `tengu_tool_brief_started` — `Brief` tool began writing brief markdown (M4-08).
pub const BRIEF_STARTED: &str = "tengu_tool_brief_started";
/// `tengu_tool_brief_completed` — `Brief` tool wrote file successfully (M4-08).
pub const BRIEF_COMPLETED: &str = "tengu_tool_brief_completed";
/// `tengu_tool_brief_failed` — `Brief` tool errored (M4-08).
pub const BRIEF_FAILED: &str = "tengu_tool_brief_failed";
/// `tengu_tool_config_started` — `Config` tool began read/write op (M4-08).
pub const CONFIG_STARTED: &str = "tengu_tool_config_started";
/// `tengu_tool_config_completed` — `Config` tool returned a result (M4-08).
pub const CONFIG_COMPLETED: &str = "tengu_tool_config_completed";
/// `tengu_tool_config_failed` — `Config` tool errored (M4-08).
pub const CONFIG_FAILED: &str = "tengu_tool_config_failed";
/// `tengu_tool_skill_started` — `Skill` tool began descriptor load (M4-08).
pub const SKILL_STARTED: &str = "tengu_tool_skill_started";
/// `tengu_tool_skill_completed` — `Skill` tool returned descriptor (M4-08).
pub const SKILL_COMPLETED: &str = "tengu_tool_skill_completed";
/// `tengu_tool_skill_failed` — `Skill` tool errored (M4-08).
pub const SKILL_FAILED: &str = "tengu_tool_skill_failed";
/// `tengu_tool_schedule_cron_started` — `ScheduleCron` tool began parsing (M4-08).
pub const SCHEDULE_CRON_STARTED: &str = "tengu_tool_schedule_cron_started";
/// `tengu_tool_schedule_cron_completed` — `ScheduleCron` tool persisted job (M4-08).
pub const SCHEDULE_CRON_COMPLETED: &str = "tengu_tool_schedule_cron_completed";
/// `tengu_tool_schedule_cron_failed` — `ScheduleCron` tool errored (M4-08).
pub const SCHEDULE_CRON_FAILED: &str = "tengu_tool_schedule_cron_failed";
/// `tengu_tool_cron_delete_started` — `CronDelete` tool began locating the job.
pub const CRON_DELETE_STARTED: &str = "tengu_tool_cron_delete_started";
/// `tengu_tool_cron_delete_completed` — `CronDelete` tool removed the descriptor.
pub const CRON_DELETE_COMPLETED: &str = "tengu_tool_cron_delete_completed";
/// `tengu_tool_cron_delete_failed` — `CronDelete` tool errored.
pub const CRON_DELETE_FAILED: &str = "tengu_tool_cron_delete_failed";
/// `tengu_tool_cron_list_started` — `CronList` tool began scanning the dir.
pub const CRON_LIST_STARTED: &str = "tengu_tool_cron_list_started";
/// `tengu_tool_cron_list_completed` — `CronList` tool returned the job list.
pub const CRON_LIST_COMPLETED: &str = "tengu_tool_cron_list_completed";
/// `tengu_tool_cron_list_failed` — `CronList` tool errored.
pub const CRON_LIST_FAILED: &str = "tengu_tool_cron_list_failed";
/// `tengu_tool_tool_search_started` — `ToolSearch` began scoring (M4-08).
pub const TOOL_SEARCH_STARTED: &str = "tengu_tool_tool_search_started";
/// `tengu_tool_tool_search_completed` — `ToolSearch` returned ranked list (M4-08).
pub const TOOL_SEARCH_COMPLETED: &str = "tengu_tool_tool_search_completed";
/// `tengu_tool_tool_search_failed` — `ToolSearch` errored (M4-08).
pub const TOOL_SEARCH_FAILED: &str = "tengu_tool_tool_search_failed";
/// `tengu_tool_remote_trigger_started` — `RemoteTrigger` stub began (M4-08).
pub const REMOTE_TRIGGER_STARTED: &str = "tengu_tool_remote_trigger_started";
/// `tengu_tool_remote_trigger_completed` — `RemoteTrigger` stub returned (M4-08).
pub const REMOTE_TRIGGER_COMPLETED: &str = "tengu_tool_remote_trigger_completed";
/// `tengu_tool_remote_trigger_failed` — `RemoteTrigger` stub errored (M4-08).
pub const REMOTE_TRIGGER_FAILED: &str = "tengu_tool_remote_trigger_failed";
/// `tengu_tool_synthetic_output_started` — `SyntheticOutput` stub began echo (M4-08).
pub const SYNTHETIC_OUTPUT_STARTED: &str = "tengu_tool_synthetic_output_started";
/// `tengu_tool_synthetic_output_completed` — `SyntheticOutput` stub returned echo (M4-08).
pub const SYNTHETIC_OUTPUT_COMPLETED: &str = "tengu_tool_synthetic_output_completed";
/// `tengu_tool_synthetic_output_failed` — `SyntheticOutput` stub errored (M4-08).
pub const SYNTHETIC_OUTPUT_FAILED: &str = "tengu_tool_synthetic_output_failed";

/// `tengu_tool_lsp_started` — `LSP` tool dispatched a request (M4-07).
pub const LSP_STARTED: &str = "tengu_tool_lsp_started";
/// `tengu_tool_lsp_completed` — `LSPTool` finished (M4-07).
pub const LSP_COMPLETED: &str = "tengu_tool_lsp_completed";
/// `tengu_tool_lsp_failed` — `LSPTool` errored (M4-07).
pub const LSP_FAILED: &str = "tengu_tool_lsp_failed";

// -- FileReadTool analytics events (3, NOT `tengu_tool_*`) ---------------------
//
// These three events use the `tengu_file_read_*` / `tengu_session_file_read`
// wire names (no `_tool_` segment) — they are emitted by FileReadTool at sites
// distinct from the generic read started/completed/failed triad above. Ported
// 1:1 from `FileReadTool.ts` (dedup short-circuit, successful text read, and
// the read-limits-override probe). They live in `FILE_READ_ANALYTICS_NAMES`
// (NOT in `NAMES`, whose entries are all `tengu_tool_*`) and are concatenated at
// the global tail of `ALL_EVENT_NAMES` (append-only convention).

/// `tengu_file_read_dedup` — Read dedup short-circuit fired (`file_unchanged`).
/// `FileReadTool.ts:559-561`. Metadata: `ext` (string, only when present) and,
/// on the SEEDED branch only (2.1.220 @235741459
/// `{source:Te("seeded"), ...b!==void 0&&{ext:b}}`), `source: "seeded"`.
/// Metadata keys are free-form; only the event NAME is registered here.
pub const FILE_READ_DEDUP: &str = "tengu_file_read_dedup";
/// `tengu_session_file_read` — a successful TEXT read completed.
/// `FileReadTool.ts:1069-1083`. Metadata: totalLines/readLines/totalBytes/
/// readBytes/offset (int), limit/ext/messageID (only when present), and the
/// `is_session_memory` / `is_session_transcript` booleans.
pub const SESSION_FILE_READ: &str = "tengu_session_file_read";
/// `tengu_file_read_limits_override` — caller overrode the default read limits.
/// `FileReadTool.ts:512-515`. Fires only when `fileReadingLimits !== undefined`;
/// the port's `ToolUseContext` has no `fileReadingLimits` field, so this branch
/// is unreachable in the port (the name is registered; the emit is a documented
/// no-op). Metadata (per TS): `hasMaxTokens` (bool), `hasMaxSizeBytes` (bool).
pub const FILE_READ_LIMITS_OVERRIDE: &str = "tengu_file_read_limits_override";

/// `tengu_file_read_reread` (#13) — fired when reading a file that ALREADY has a
/// read-file-state entry, BEFORE the dedup short-circuit (claude-code:
/// 2.1.220 @235740900:
/// `if(m)M("tengu_file_read_reread",{priorOp:Te(m.seededFromContext?"seeded"
/// :m.offset===void 0?"edit_write":"read")})`). Metadata: `priorOp` =
/// `"seeded"` for a memory-seeded entry (tested FIRST), `"read"` when the prior
/// entry came from a Read, `"edit_write"` when it came from an Edit/Write.
pub const FILE_READ_REREAD: &str = "tengu_file_read_reread";

/// Order-locked array of tool event names; consumed by `tengu::ALL_EVENT_NAMES`.
/// M3-06 locked the first 40 (less the 6 grep/glob events later removed to
/// match claude-code, which emits no `tengu_tool_grep_*`/`tengu_tool_glob_*`);
/// M4-02 appended 9 (powershell/repl/sleep);
/// M4-03 appended 3 (`web_search`); M4-04 appended 15 workflow events;
/// M4-05 appended 24 agent/task events; M4-06 appends 6 team events;
/// M4-07 appends 13 MCP + LSP events; M4-08 appends 24 system events;
/// CronDelete/CronList append 6 (2 tools × 3 lifecycle stages). The 3
/// `FileReadTool` analytics names are NOT in this array (they are not
/// `tengu_tool_*`); see [`FILE_READ_ANALYTICS_NAMES`].
pub(crate) const NAMES: &[&str] = &[
    STARTED,
    COMPLETED,
    FAILED,
    CANCELLED,
    PERMISSION_REQUESTED,
    PERMISSION_GRANTED,
    PERMISSION_DENIED,
    PERMISSION_REMEMBERED,
    BASH_STARTED,
    BASH_COMPLETED,
    BASH_FAILED,
    BASH_TIMEOUT,
    EDIT_STARTED,
    EDIT_COMPLETED,
    EDIT_FAILED,
    READ_STARTED,
    READ_COMPLETED,
    READ_FAILED,
    WRITE_STARTED,
    WRITE_COMPLETED,
    WRITE_FAILED,
    WEB_FETCH_STARTED,
    WEB_FETCH_COMPLETED,
    WEB_FETCH_FAILED,
    WEB_SEARCH_STARTED,
    WEB_SEARCH_COMPLETED,
    WEB_SEARCH_FAILED,
    TASK_DISPATCHED,
    TASK_COMPLETED,
    TASK_FAILED,
    NOTEBOOK_STARTED,
    NOTEBOOK_COMPLETED,
    NOTEBOOK_FAILED,
    MCP_INVOKED,
    MCP_COMPLETED,
    MCP_FAILED,
    SKILL_INVOKED,
    POWERSHELL_STARTED,
    POWERSHELL_COMPLETED,
    POWERSHELL_FAILED,
    REPL_STARTED,
    REPL_COMPLETED,
    REPL_FAILED,
    SLEEP_STARTED,
    SLEEP_COMPLETED,
    SLEEP_FAILED,
    // M4-04 Workflow tools (TodoWrite per-tool events removed for 2.1.195 parity)
    ENTER_PLAN_MODE_STARTED,
    ENTER_PLAN_MODE_COMPLETED,
    ENTER_PLAN_MODE_FAILED,
    EXIT_PLAN_MODE_STARTED,
    EXIT_PLAN_MODE_COMPLETED,
    EXIT_PLAN_MODE_FAILED,
    ENTER_WORKTREE_STARTED,
    ENTER_WORKTREE_COMPLETED,
    ENTER_WORKTREE_FAILED,
    WORKTREE_CREATED,
    WORKTREE_ENTERED_EXISTING,
    EXIT_WORKTREE_STARTED,
    EXIT_WORKTREE_COMPLETED,
    EXIT_WORKTREE_FAILED,
    WORKTREE_KEPT,
    WORKTREE_REMOVED,
    // M4-05 Agent + Task tools (24 events, 8 tools × 3 lifecycle stages)
    AGENT_STARTED,
    AGENT_COMPLETED_M4_05,
    AGENT_FAILED,
    TASK_CREATE_STARTED,
    TASK_CREATE_COMPLETED,
    TASK_CREATE_FAILED,
    TASK_GET_STARTED,
    TASK_GET_COMPLETED,
    TASK_GET_FAILED,
    TASK_LIST_STARTED,
    TASK_LIST_COMPLETED,
    TASK_LIST_FAILED,
    TASK_UPDATE_STARTED,
    TASK_UPDATE_COMPLETED,
    TASK_UPDATE_FAILED,
    TASK_STOP_STARTED,
    TASK_STOP_COMPLETED,
    TASK_STOP_FAILED,
    TASK_OUTPUT_STARTED,
    TASK_OUTPUT_COMPLETED,
    TASK_OUTPUT_FAILED,
    SEND_MESSAGE_STARTED,
    SEND_MESSAGE_COMPLETED,
    SEND_MESSAGE_FAILED,
    // M4-07 MCP + LSP tools (13 NEW events; MCP_COMPLETED + MCP_FAILED are
    // M3-06 baseline above and already in this array — they cover MCPTool's
    // completed/failed lifecycle states).
    MCP_STARTED,
    MCP_AUTH_STARTED,
    MCP_AUTH_COMPLETED,
    MCP_AUTH_FAILED,
    LIST_MCP_RESOURCES_STARTED,
    LIST_MCP_RESOURCES_COMPLETED,
    LIST_MCP_RESOURCES_FAILED,
    READ_MCP_RESOURCE_STARTED,
    READ_MCP_RESOURCE_COMPLETED,
    READ_MCP_RESOURCE_FAILED,
    LSP_STARTED,
    LSP_COMPLETED,
    LSP_FAILED,
    // M4-08 System tools (24 events, 8 tools × 3 lifecycle stages)
    ASK_USER_QUESTION_STARTED,
    ASK_USER_QUESTION_COMPLETED,
    ASK_USER_QUESTION_FAILED,
    BRIEF_STARTED,
    BRIEF_COMPLETED,
    BRIEF_FAILED,
    CONFIG_STARTED,
    CONFIG_COMPLETED,
    CONFIG_FAILED,
    SKILL_STARTED,
    SKILL_COMPLETED,
    SKILL_FAILED,
    SCHEDULE_CRON_STARTED,
    SCHEDULE_CRON_COMPLETED,
    SCHEDULE_CRON_FAILED,
    // CronDelete / CronList lifecycle events (6, 2 tools × 3 stages)
    CRON_DELETE_STARTED,
    CRON_DELETE_COMPLETED,
    CRON_DELETE_FAILED,
    CRON_LIST_STARTED,
    CRON_LIST_COMPLETED,
    CRON_LIST_FAILED,
    TOOL_SEARCH_STARTED,
    TOOL_SEARCH_COMPLETED,
    TOOL_SEARCH_FAILED,
    REMOTE_TRIGGER_STARTED,
    REMOTE_TRIGGER_COMPLETED,
    REMOTE_TRIGGER_FAILED,
    SYNTHETIC_OUTPUT_STARTED,
    SYNTHETIC_OUTPUT_COMPLETED,
    SYNTHETIC_OUTPUT_FAILED,
];

/// The 3 `FileReadTool` analytics names (NOT `tengu_tool_*`), kept OUT of
/// [`NAMES`] so the `tengu_tool_*` prefix invariant of the tool concat block in
/// [`crate::tengu::ALL_EVENT_NAMES`] is preserved. They are concatenated at the
/// GLOBAL TAIL of `ALL_EVENT_NAMES` (after the tui block) by `tengu::mod.rs`, so
/// every existing per-block prefix slice stays valid and only the registry tail
/// grows by 4. See `tengu_events.json` (positions 336/337/338/339).
pub(crate) const FILE_READ_ANALYTICS_NAMES: &[&str] = &[
    FILE_READ_DEDUP,
    SESSION_FILE_READ,
    FILE_READ_LIMITS_OVERRIDE,
    FILE_READ_REREAD,
];

#[cfg(test)]
mod m4_04_workflow_event_tests {
    use super::*;

    #[test]
    fn enter_plan_mode_constants_are_locked() {
        assert_eq!(
            ENTER_PLAN_MODE_STARTED,
            "tengu_tool_enter_plan_mode_started"
        );
        assert_eq!(
            ENTER_PLAN_MODE_COMPLETED,
            "tengu_tool_enter_plan_mode_completed"
        );
        assert_eq!(ENTER_PLAN_MODE_FAILED, "tengu_tool_enter_plan_mode_failed");
    }

    #[test]
    fn exit_plan_mode_constants_are_locked() {
        assert_eq!(EXIT_PLAN_MODE_STARTED, "tengu_tool_exit_plan_mode_started");
        assert_eq!(
            EXIT_PLAN_MODE_COMPLETED,
            "tengu_tool_exit_plan_mode_completed"
        );
        assert_eq!(EXIT_PLAN_MODE_FAILED, "tengu_tool_exit_plan_mode_failed");
    }

    #[test]
    fn enter_worktree_constants_are_locked() {
        assert_eq!(ENTER_WORKTREE_STARTED, "tengu_tool_enter_worktree_started");
        assert_eq!(
            ENTER_WORKTREE_COMPLETED,
            "tengu_tool_enter_worktree_completed"
        );
        assert_eq!(ENTER_WORKTREE_FAILED, "tengu_tool_enter_worktree_failed");
    }

    #[test]
    fn worktree_206_byte_exact_events_are_locked() {
        assert_eq!(WORKTREE_CREATED, "tengu_worktree_created");
        assert_eq!(WORKTREE_ENTERED_EXISTING, "tengu_worktree_entered_existing");
    }

    #[test]
    fn exit_worktree_constants_are_locked() {
        assert_eq!(EXIT_WORKTREE_STARTED, "tengu_tool_exit_worktree_started");
        assert_eq!(
            EXIT_WORKTREE_COMPLETED,
            "tengu_tool_exit_worktree_completed"
        );
        assert_eq!(EXIT_WORKTREE_FAILED, "tengu_tool_exit_worktree_failed");
    }

    #[test]
    fn exit_worktree_206_byte_exact_events_are_locked() {
        assert_eq!(WORKTREE_KEPT, "tengu_worktree_kept");
        assert_eq!(WORKTREE_REMOVED, "tengu_worktree_removed");
    }

    #[test]
    fn names_array_contains_all_workflow_events() {
        let workflow = [
            ENTER_PLAN_MODE_STARTED,
            ENTER_PLAN_MODE_COMPLETED,
            ENTER_PLAN_MODE_FAILED,
            EXIT_PLAN_MODE_STARTED,
            EXIT_PLAN_MODE_COMPLETED,
            EXIT_PLAN_MODE_FAILED,
            ENTER_WORKTREE_STARTED,
            ENTER_WORKTREE_COMPLETED,
            ENTER_WORKTREE_FAILED,
            WORKTREE_CREATED,
            WORKTREE_ENTERED_EXISTING,
            EXIT_WORKTREE_STARTED,
            EXIT_WORKTREE_COMPLETED,
            EXIT_WORKTREE_FAILED,
            WORKTREE_KEPT,
            WORKTREE_REMOVED,
        ];
        for name in workflow {
            assert!(
                NAMES.contains(&name),
                "NAMES array missing workflow event: {name}"
            );
        }
        assert_eq!(
            NAMES.len(),
            129,
            "M3-06 34 (40 baseline − 6 grep/glob removed to match claude-code) + M4-02 9 + M4-03 3 + M4-04 12 (TodoWrite 3 removed for 2.1.195 parity) + M4-05 24 + M4-06 6 + M4-07 13 + M4-08 24 + cron_delete/cron_list 6 + worktree-206-parity 2 (WORKTREE_CREATED/WORKTREE_ENTERED_EXISTING) + exit-worktree-206-parity 2 (WORKTREE_KEPT/WORKTREE_REMOVED) = 135, − 6 team_create/team_delete retired with the explicit team tools (bd0abff79) = 129 (FileReadTool analytics 4 live in FILE_READ_ANALYTICS_NAMES, concatenated at the registry tail)"
        );
    }

    /// The six names `bd0abff79` retired, pinned as ABSENT.
    ///
    /// 2.1.263 replaced the explicit TeamCreate / TeamDelete tools with a single
    /// implicit team, and the tools went with them (`tools/team/` no longer
    /// exists). The count above dropped 135 → 129 for exactly this reason and
    /// nothing else.
    ///
    /// A bare count cannot tell "six were correctly retired" from "six went
    /// missing", and updating the arithmetic without saying which six is how the
    /// comment above went stale in the first place. Assert the membership, not
    /// just the total: if one of these ever comes back, that is a decision
    /// someone should make on purpose.
    #[test]
    fn the_retired_team_tool_events_stay_retired() {
        for name in [
            "tengu_tool_team_create_started",
            "tengu_tool_team_create_completed",
            "tengu_tool_team_create_failed",
            "tengu_tool_team_delete_started",
            "tengu_tool_team_delete_completed",
            "tengu_tool_team_delete_failed",
        ] {
            assert!(
                !NAMES.contains(&name),
                "{name} belongs to a tool this build does not ship"
            );
        }
    }
}

#[cfg(test)]
mod cron_delete_list_event_tests {
    use super::*;

    #[test]
    fn cron_delete_list_constants_are_locked() {
        assert_eq!(CRON_DELETE_STARTED, "tengu_tool_cron_delete_started");
        assert_eq!(CRON_DELETE_COMPLETED, "tengu_tool_cron_delete_completed");
        assert_eq!(CRON_DELETE_FAILED, "tengu_tool_cron_delete_failed");
        assert_eq!(CRON_LIST_STARTED, "tengu_tool_cron_list_started");
        assert_eq!(CRON_LIST_COMPLETED, "tengu_tool_cron_list_completed");
        assert_eq!(CRON_LIST_FAILED, "tengu_tool_cron_list_failed");
    }

    #[test]
    fn names_array_contains_all_6_cron_events() {
        for name in [
            CRON_DELETE_STARTED,
            CRON_DELETE_COMPLETED,
            CRON_DELETE_FAILED,
            CRON_LIST_STARTED,
            CRON_LIST_COMPLETED,
            CRON_LIST_FAILED,
        ] {
            assert!(
                NAMES.contains(&name),
                "NAMES array missing cron delete/list event: {name}"
            );
        }
    }
}

#[cfg(test)]
mod m4_08_system_event_tests {
    use super::*;

    #[test]
    fn system_constants_are_locked() {
        assert_eq!(
            ASK_USER_QUESTION_STARTED,
            "tengu_tool_ask_user_question_started"
        );
        assert_eq!(
            ASK_USER_QUESTION_COMPLETED,
            "tengu_tool_ask_user_question_completed"
        );
        assert_eq!(
            ASK_USER_QUESTION_FAILED,
            "tengu_tool_ask_user_question_failed"
        );
        assert_eq!(BRIEF_STARTED, "tengu_tool_brief_started");
        assert_eq!(BRIEF_COMPLETED, "tengu_tool_brief_completed");
        assert_eq!(BRIEF_FAILED, "tengu_tool_brief_failed");
        assert_eq!(CONFIG_STARTED, "tengu_tool_config_started");
        assert_eq!(CONFIG_COMPLETED, "tengu_tool_config_completed");
        assert_eq!(CONFIG_FAILED, "tengu_tool_config_failed");
        assert_eq!(SKILL_STARTED, "tengu_tool_skill_started");
        assert_eq!(SKILL_COMPLETED, "tengu_tool_skill_completed");
        assert_eq!(SKILL_FAILED, "tengu_tool_skill_failed");
        assert_eq!(SCHEDULE_CRON_STARTED, "tengu_tool_schedule_cron_started");
        assert_eq!(
            SCHEDULE_CRON_COMPLETED,
            "tengu_tool_schedule_cron_completed"
        );
        assert_eq!(SCHEDULE_CRON_FAILED, "tengu_tool_schedule_cron_failed");
        assert_eq!(TOOL_SEARCH_STARTED, "tengu_tool_tool_search_started");
        assert_eq!(TOOL_SEARCH_COMPLETED, "tengu_tool_tool_search_completed");
        assert_eq!(TOOL_SEARCH_FAILED, "tengu_tool_tool_search_failed");
        assert_eq!(REMOTE_TRIGGER_STARTED, "tengu_tool_remote_trigger_started");
        assert_eq!(
            REMOTE_TRIGGER_COMPLETED,
            "tengu_tool_remote_trigger_completed"
        );
        assert_eq!(REMOTE_TRIGGER_FAILED, "tengu_tool_remote_trigger_failed");
        assert_eq!(
            SYNTHETIC_OUTPUT_STARTED,
            "tengu_tool_synthetic_output_started"
        );
        assert_eq!(
            SYNTHETIC_OUTPUT_COMPLETED,
            "tengu_tool_synthetic_output_completed"
        );
        assert_eq!(
            SYNTHETIC_OUTPUT_FAILED,
            "tengu_tool_synthetic_output_failed"
        );
    }

    #[test]
    fn names_array_contains_all_24_m4_08_events() {
        let system = [
            ASK_USER_QUESTION_STARTED,
            ASK_USER_QUESTION_COMPLETED,
            ASK_USER_QUESTION_FAILED,
            BRIEF_STARTED,
            BRIEF_COMPLETED,
            BRIEF_FAILED,
            CONFIG_STARTED,
            CONFIG_COMPLETED,
            CONFIG_FAILED,
            SKILL_STARTED,
            SKILL_COMPLETED,
            SKILL_FAILED,
            SCHEDULE_CRON_STARTED,
            SCHEDULE_CRON_COMPLETED,
            SCHEDULE_CRON_FAILED,
            TOOL_SEARCH_STARTED,
            TOOL_SEARCH_COMPLETED,
            TOOL_SEARCH_FAILED,
            REMOTE_TRIGGER_STARTED,
            REMOTE_TRIGGER_COMPLETED,
            REMOTE_TRIGGER_FAILED,
            SYNTHETIC_OUTPUT_STARTED,
            SYNTHETIC_OUTPUT_COMPLETED,
            SYNTHETIC_OUTPUT_FAILED,
        ];
        for name in system {
            assert!(
                NAMES.contains(&name),
                "NAMES array missing M4-08 event: {name}"
            );
        }
    }
}

#[cfg(test)]
mod m4_07_mcp_lsp_event_tests {
    use super::*;

    #[test]
    fn mcp_lsp_constants_are_locked() {
        assert_eq!(MCP_STARTED, "tengu_tool_mcp_started");
        assert_eq!(MCP_AUTH_STARTED, "tengu_tool_mcp_auth_started");
        assert_eq!(MCP_AUTH_COMPLETED, "tengu_tool_mcp_auth_completed");
        assert_eq!(MCP_AUTH_FAILED, "tengu_tool_mcp_auth_failed");
        assert_eq!(
            LIST_MCP_RESOURCES_STARTED,
            "tengu_tool_list_mcp_resources_started"
        );
        assert_eq!(
            LIST_MCP_RESOURCES_COMPLETED,
            "tengu_tool_list_mcp_resources_completed"
        );
        assert_eq!(
            LIST_MCP_RESOURCES_FAILED,
            "tengu_tool_list_mcp_resources_failed"
        );
        assert_eq!(
            READ_MCP_RESOURCE_STARTED,
            "tengu_tool_read_mcp_resource_started"
        );
        assert_eq!(
            READ_MCP_RESOURCE_COMPLETED,
            "tengu_tool_read_mcp_resource_completed"
        );
        assert_eq!(
            READ_MCP_RESOURCE_FAILED,
            "tengu_tool_read_mcp_resource_failed"
        );
        assert_eq!(LSP_STARTED, "tengu_tool_lsp_started");
        assert_eq!(LSP_COMPLETED, "tengu_tool_lsp_completed");
        assert_eq!(LSP_FAILED, "tengu_tool_lsp_failed");
    }

    #[test]
    fn names_array_contains_all_13_m4_07_events() {
        let mcp_lsp = [
            MCP_STARTED,
            MCP_AUTH_STARTED,
            MCP_AUTH_COMPLETED,
            MCP_AUTH_FAILED,
            LIST_MCP_RESOURCES_STARTED,
            LIST_MCP_RESOURCES_COMPLETED,
            LIST_MCP_RESOURCES_FAILED,
            READ_MCP_RESOURCE_STARTED,
            READ_MCP_RESOURCE_COMPLETED,
            READ_MCP_RESOURCE_FAILED,
            LSP_STARTED,
            LSP_COMPLETED,
            LSP_FAILED,
        ];
        for name in mcp_lsp {
            assert!(
                NAMES.contains(&name),
                "NAMES array missing M4-07 event: {name}"
            );
        }
    }
}

#[cfg(test)]
mod m4_05_agent_task_event_tests {
    use super::*;

    #[test]
    fn agent_constants_are_locked() {
        assert_eq!(AGENT_STARTED, "tengu_tool_agent_started");
        assert_eq!(AGENT_COMPLETED_M4_05, "tengu_tool_agent_completed");
        assert_eq!(AGENT_FAILED, "tengu_tool_agent_failed");
    }

    #[test]
    fn task_create_constants_are_locked() {
        assert_eq!(TASK_CREATE_STARTED, "tengu_tool_task_create_started");
        assert_eq!(TASK_CREATE_COMPLETED, "tengu_tool_task_create_completed");
        assert_eq!(TASK_CREATE_FAILED, "tengu_tool_task_create_failed");
    }

    #[test]
    fn task_get_list_update_stop_output_constants_are_locked() {
        assert_eq!(TASK_GET_STARTED, "tengu_tool_task_get_started");
        assert_eq!(TASK_GET_COMPLETED, "tengu_tool_task_get_completed");
        assert_eq!(TASK_GET_FAILED, "tengu_tool_task_get_failed");
        assert_eq!(TASK_LIST_STARTED, "tengu_tool_task_list_started");
        assert_eq!(TASK_LIST_COMPLETED, "tengu_tool_task_list_completed");
        assert_eq!(TASK_LIST_FAILED, "tengu_tool_task_list_failed");
        assert_eq!(TASK_UPDATE_STARTED, "tengu_tool_task_update_started");
        assert_eq!(TASK_UPDATE_COMPLETED, "tengu_tool_task_update_completed");
        assert_eq!(TASK_UPDATE_FAILED, "tengu_tool_task_update_failed");
        assert_eq!(TASK_STOP_STARTED, "tengu_tool_task_stop_started");
        assert_eq!(TASK_STOP_COMPLETED, "tengu_tool_task_stop_completed");
        assert_eq!(TASK_STOP_FAILED, "tengu_tool_task_stop_failed");
        assert_eq!(TASK_OUTPUT_STARTED, "tengu_tool_task_output_started");
        assert_eq!(TASK_OUTPUT_COMPLETED, "tengu_tool_task_output_completed");
        assert_eq!(TASK_OUTPUT_FAILED, "tengu_tool_task_output_failed");
    }

    #[test]
    fn send_message_constants_are_locked() {
        assert_eq!(SEND_MESSAGE_STARTED, "tengu_tool_send_message_started");
        assert_eq!(SEND_MESSAGE_COMPLETED, "tengu_tool_send_message_completed");
        assert_eq!(SEND_MESSAGE_FAILED, "tengu_tool_send_message_failed");
    }

    #[test]
    fn names_array_contains_all_24_agent_task_events() {
        let agent_task = [
            AGENT_STARTED,
            AGENT_COMPLETED_M4_05,
            AGENT_FAILED,
            TASK_CREATE_STARTED,
            TASK_CREATE_COMPLETED,
            TASK_CREATE_FAILED,
            TASK_GET_STARTED,
            TASK_GET_COMPLETED,
            TASK_GET_FAILED,
            TASK_LIST_STARTED,
            TASK_LIST_COMPLETED,
            TASK_LIST_FAILED,
            TASK_UPDATE_STARTED,
            TASK_UPDATE_COMPLETED,
            TASK_UPDATE_FAILED,
            TASK_STOP_STARTED,
            TASK_STOP_COMPLETED,
            TASK_STOP_FAILED,
            TASK_OUTPUT_STARTED,
            TASK_OUTPUT_COMPLETED,
            TASK_OUTPUT_FAILED,
            SEND_MESSAGE_STARTED,
            SEND_MESSAGE_COMPLETED,
            SEND_MESSAGE_FAILED,
        ];
        for name in agent_task {
            assert!(
                NAMES.contains(&name),
                "NAMES array missing M4-05 event: {name}"
            );
        }
        // Distinctness across the entire NAMES array.
        let mut seen = std::collections::HashSet::new();
        for n in NAMES {
            assert!(seen.insert(*n), "duplicate event name in NAMES: {n}");
        }
    }
}

#[cfg(test)]
mod file_read_analytics_event_tests {
    use super::*;

    #[test]
    fn file_read_analytics_constants_are_locked() {
        assert_eq!(FILE_READ_DEDUP, "tengu_file_read_dedup");
        assert_eq!(SESSION_FILE_READ, "tengu_session_file_read");
        assert_eq!(FILE_READ_LIMITS_OVERRIDE, "tengu_file_read_limits_override");
        assert_eq!(FILE_READ_REREAD, "tengu_file_read_reread");
    }

    #[test]
    fn file_read_analytics_array_contains_all_4_events() {
        assert_eq!(FILE_READ_ANALYTICS_NAMES.len(), 4);
        for name in [
            FILE_READ_DEDUP,
            SESSION_FILE_READ,
            FILE_READ_LIMITS_OVERRIDE,
            FILE_READ_REREAD,
        ] {
            assert!(
                FILE_READ_ANALYTICS_NAMES.contains(&name),
                "FILE_READ_ANALYTICS_NAMES missing event: {name}"
            );
            // They are NOT `tengu_tool_*` events — assert the distinct family,
            // and confirm they are kept OUT of the `tengu_tool_*` NAMES block.
            assert!(!name.starts_with("tengu_tool_"));
            assert!(!NAMES.contains(&name));
        }
    }
}

// -- Shared enums -------------------------------------------------------------

/// Generic tool-permission outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionDecision {
    /// User explicitly allowed this single invocation.
    Allow,
    /// User explicitly denied this invocation.
    Deny,
    /// Allowed once with no persistence.
    AllowOnce,
    /// Allowed and remembered for the active scope.
    AllowAlways,
}

/// Where a remembered permission persists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionScope {
    /// Lives only for the active session.
    Session,
    /// Persists to the project-level settings.
    Project,
    /// Persists to the user-global settings.
    User,
}

/// Generic tool failure classification (no PII).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolFailureKind {
    /// Input failed validation before dispatch.
    InvalidInput,
    /// Permission gate refused the action.
    PermissionDenied,
    /// Tool ran past its timeout watchdog.
    Timeout,
    /// Underlying IO or syscall error.
    IoError,
    /// Sandbox runtime refused the operation.
    SandboxRefused,
    /// Catch-all bucket for everything else.
    Other,
}

// -- Generic lifecycle payloads -----------------------------------------------

/// Payload for [`STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted tool name (e.g. `bash`, `edit`, `mcp__server__method`).
    pub tool_name: Verified,
}

/// Payload for [`COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted tool name.
    pub tool_name: Verified,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted tool name.
    pub tool_name: Verified,
    /// Coarse failure classification.
    pub failure_kind: ToolFailureKind,
}

/// Payload for [`CANCELLED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelledPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted tool name.
    pub tool_name: Verified,
}

// -- Permission gate payloads -------------------------------------------------

/// Payload for [`PERMISSION_REQUESTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequestedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted tool name.
    pub tool_name: Verified,
    /// Whitelisted action (e.g. `read`, `write`, `network`).
    pub action: Verified,
}

/// Payload for [`PERMISSION_GRANTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionGrantedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Outcome of the prompt.
    pub decision: PermissionDecision,
}

/// Payload for [`PERMISSION_DENIED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionDeniedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted denial reason (no PII).
    pub reason: Verified,
}

/// Payload for [`PERMISSION_REMEMBERED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRememberedPayload {
    /// Whitelisted tool name the rule applies to.
    pub tool_name: Verified,
    /// Whitelisted action the rule applies to.
    pub action: Verified,
    /// Where the decision was persisted.
    pub scope: PermissionScope,
}

// -- Bash payloads ------------------------------------------------------------

/// Payload for [`BASH_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashStartedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// The command string is PII-tagged (may include filepaths / secrets).
    pub command: PiiTagged,
    /// Configured timeout for this invocation, in milliseconds.
    pub timeout_ms: u64,
}

/// Payload for [`BASH_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Process exit code (negative for signal-terminated).
    pub exit_code: i32,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
    /// Bytes captured on stdout.
    pub stdout_bytes: u64,
    /// Bytes captured on stderr.
    pub stderr_bytes: u64,
}

/// Payload for [`BASH_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted error description (no PII).
    pub error: Verified,
}

/// Payload for [`BASH_TIMEOUT`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashTimeoutPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Configured timeout that was hit, in milliseconds.
    pub timeout_ms: u64,
}

// -- Edit payloads ------------------------------------------------------------

/// Payload for [`EDIT_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditStartedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// PII-tagged: filepath (may carry user directory names).
    pub file_path: PiiTagged,
}

/// Payload for [`EDIT_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Number of replacement matches applied.
    pub replacements: u32,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`EDIT_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Coarse failure classification.
    pub failure_kind: ToolFailureKind,
}

// -- Read payloads ------------------------------------------------------------

/// Payload for [`READ_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadStartedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// PII-tagged: filepath.
    pub file_path: PiiTagged,
}

/// Payload for [`READ_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Total bytes read from the file.
    pub bytes_read: u64,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`READ_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Coarse failure classification.
    pub failure_kind: ToolFailureKind,
}

// -- Write payloads -----------------------------------------------------------

/// Payload for [`WRITE_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteStartedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// PII-tagged: filepath.
    pub file_path: PiiTagged,
}

/// Payload for [`WRITE_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Total bytes written to the file.
    pub bytes_written: u64,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`WRITE_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Coarse failure classification.
    pub failure_kind: ToolFailureKind,
}

// Grep/Glob emit NO telemetry (claude-code v2.1.183 emits no
// `tengu_tool_grep_*` / `tengu_tool_glob_*` events), so there are no Grep/Glob
// payload structs.

// -- WebFetch payloads --------------------------------------------------------

/// Payload for [`WEB_FETCH_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchStartedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// PII-tagged: URL may include tokens or org-internal hostnames.
    pub url: PiiTagged,
}

/// Payload for [`WEB_FETCH_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// HTTP response status code.
    pub status: u32,
    /// Total bytes received in the response body.
    pub bytes_received: u64,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`WEB_FETCH_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebFetchFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Coarse failure classification.
    pub failure_kind: ToolFailureKind,
}

// -- Task payloads ------------------------------------------------------------

/// Payload for [`TASK_DISPATCHED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDispatchedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted subagent kind (`general-purpose`, `output-style-setup`, etc.).
    pub subagent_kind: Verified,
}

/// Payload for [`TASK_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Wall-clock duration of the subagent in milliseconds.
    pub duration_ms: u64,
    /// Turns the subagent ran before returning.
    pub turns: u32,
}

/// Payload for [`TASK_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted error description (no PII).
    pub error: Verified,
}

// -- Notebook payloads --------------------------------------------------------

/// Payload for [`NOTEBOOK_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotebookStartedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// PII-tagged: notebook filepath.
    pub notebook_path: PiiTagged,
}

/// Payload for [`NOTEBOOK_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotebookCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Number of cells modified.
    pub cells_edited: u32,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`NOTEBOOK_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotebookFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Coarse failure classification.
    pub failure_kind: ToolFailureKind,
}

// -- MCP payloads -------------------------------------------------------------

/// Payload for [`MCP_INVOKED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpInvokedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted MCP server name.
    pub server_name: Verified,
    /// Whitelisted MCP tool / method name.
    pub method: Verified,
}

/// Payload for [`MCP_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCompletedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted MCP server name.
    pub server_name: Verified,
    /// Wall-clock duration of the tool call in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`MCP_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpFailedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted MCP server name.
    pub server_name: Verified,
    /// Coarse failure classification.
    pub failure_kind: ToolFailureKind,
}

// -- Skill payload ------------------------------------------------------------

/// Payload for [`SKILL_INVOKED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillInvokedPayload {
    /// Stable identifier for this tool invocation.
    pub invocation_id: Verified,
    /// Whitelisted skill name.
    pub skill_name: Verified,
}
