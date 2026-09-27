//! LSP tool. Extracted in M8-P7. Desktop-only (spawns language servers).
//! The impl module is `lsp_tool` (not `lsp`) to avoid colliding with the
//! extern `lsp` crate this tool depends on.
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

pub mod lsp_tool;
pub use lsp_tool::LSPTool;
/// Register the LSP tool against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    register_all_with_live_cwd(reg, ctx, None);
}

/// Register the LSP tool, injecting an optional shared live-cwd cell
/// (claude-code `getCwd()`/`Ct()`) so relative `filePath` expansion and the
/// gitignore-filter root follow a Bash `cd`. When `None` (offline factory), the
/// tool falls back to the process cwd — byte-identical to [`register_all`].
pub fn register_all_with_live_cwd(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    live_cwd: Option<tool_api::LiveCwdCell>,
) {
    use std::sync::Arc;
    let tool = match live_cwd {
        Some(cell) => LSPTool::new(ctx).with_live_cwd(cell),
        None => LSPTool::new(ctx),
    };
    reg.register_builtin(Arc::new(tool));
}
