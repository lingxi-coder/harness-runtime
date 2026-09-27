//! Worktree tools: EnterWorktree, ExitWorktree. Extracted in M8-P7.
//! Desktop-only (git worktree management).
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
// Documentation debt, not a decision that docs do not matter: this crate had
// 1 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod worktree;
pub use worktree::{EnterWorktreeTool, ExitWorktreeTool};
/// Register the worktree enter/exit tools against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    register_all_with_persister(reg, ctx, None);
}

/// Register the worktree enter/exit tools with an optional transcript persister
/// (parity 2.1.212's `saveWorktreeState`). When `persister` is `Some`, a
/// successful `EnterWorktree` writes a `worktree-state` entry and `ExitWorktree`
/// writes the clear record, so a later `--continue`/`--resume` can rehydrate the
/// active worktree. `None` keeps the pre-persist behavior (offline factory,
/// `--no-session-persistence`, tests).
pub fn register_all_with_persister(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    persister: Option<std::sync::Arc<dyn tool_api::WorktreeStatePersister>>,
) {
    use std::sync::Arc;
    let enter = match &persister {
        Some(p) => EnterWorktreeTool::new(ctx.clone()).with_state_persister(p.clone()),
        None => EnterWorktreeTool::new(ctx.clone()),
    };
    let exit = match persister {
        Some(p) => ExitWorktreeTool::new(ctx).with_state_persister(p),
        None => ExitWorktreeTool::new(ctx),
    };
    reg.register_builtin(Arc::new(enter));
    reg.register_builtin(Arc::new(exit));
}
