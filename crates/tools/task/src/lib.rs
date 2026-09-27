//! Task lifecycle tools: TaskCreate/Get/List/Update/Stop/Output + TodoWrite.
//!
//! Extracted from the `tools` monolith in M8-P7. Cross-platform (they
//! dispatch through `TaskRegistryHandle` carried in `BuiltinToolContext`).

#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::match_wildcard_for_single_variants,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::format_collect,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::manual_let_else
)]
// Dead code kept visible, not swept: this crate had 2 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

pub mod monitor;
pub mod reminder;
pub mod task;
pub mod todo_write;

// The V2 todo store + its embedded proper-lockfile port moved to the leaf
// `task-store` crate (so `tasks`/`coordinator` can reach them without the
// `tool-task → cron → tasks` cycle). Re-exported under the original paths so
// existing consumers (`orchestrator`, engine roots, tests) compile unchanged.
pub use task_store::{proper_lockfile, todo_store};

pub use monitor::MonitorTool;
pub use task::{
    TaskCreateTool, TaskGetTool, TaskListTool, TaskOutputTool, TaskStopTool, TaskUpdateTool,
};
pub use todo_write::TodoWriteTool;

/// Register the 6 task tools + TodoWrite against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(TaskCreateTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskGetTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskListTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskUpdateTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskStopTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskOutputTool::new(ctx.clone())));
    // PARITY: the `Monitor` tool (binary `EVp`). Registered always; isEnabled
    // (flag `tengu_amber_sentinel`, default off) gates exposure — invisible to the
    // model by default, like the shipped binary.
    reg.register_builtin(Arc::new(MonitorTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TodoWriteTool::new(ctx)));
}

/// Register the task/todo tools that have portable mobile execution semantics.
///
/// `Monitor` intentionally stays desktop-only: its command contract is Bash,
/// including process substitution and background-shell behavior that the
/// restricted mobile `Shell` carrier does not implement.
pub fn register_mobile(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(TaskCreateTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskGetTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskListTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskUpdateTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskStopTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskOutputTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TodoWriteTool::new(ctx)));
}

/// Register the portable mobile task surface against the host's app-private
/// config home instead of process-global `HOME`.
pub fn register_mobile_with_config_home(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    config_home: std::path::PathBuf,
) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(
        TaskCreateTool::new(ctx.clone()).with_config_home(config_home.clone()),
    ));
    reg.register_builtin(Arc::new(
        TaskGetTool::new(ctx.clone()).with_config_home(config_home.clone()),
    ));
    reg.register_builtin(Arc::new(
        TaskListTool::new(ctx.clone()).with_config_home(config_home.clone()),
    ));
    reg.register_builtin(Arc::new(
        TaskUpdateTool::new(ctx.clone()).with_config_home(config_home),
    ));
    reg.register_builtin(Arc::new(TaskStopTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TaskOutputTool::new(ctx.clone())));
    reg.register_builtin(Arc::new(TodoWriteTool::new(ctx)));
}
