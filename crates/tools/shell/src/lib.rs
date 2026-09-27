//! Shell execution tools: Bash, PowerShell, REPL.
//!
//! Extracted from the `tools` monolith in M8-P5. Desktop-only (they need
//! process spawning + a sandbox); the mobile composition root never
//! registers them. Each takes a `tool_api::BuiltinToolContext`; `register_all`
//! wires all three. `shared` holds the ANSI-stripping helper used by all
//! three; cross-tool helpers live in `tool_api::util`.

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
// Dead code kept visible, not swept: this crate had 1 item(s) rustc could
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

pub mod bash;
pub mod bash_edit_diff;
pub mod command_semantics;
pub mod powershell;
pub mod powershell_semantics;
pub mod prompt;
pub mod read_only;
pub mod repl;
pub mod search_read;
pub mod shared;
pub mod silent;

#[cfg(test)]
pub(crate) mod test_path_env;

pub use bash::BashTool;
pub use powershell::PowerShellTool;
pub use repl::REPLTool;

/// Register Bash, PowerShell, and REPL tools against `reg`.
///
/// The `BashTool` is registered with NO `CwdChanged` hook firer (a strict
/// no-op). Desktop runtimes that want the `CwdChanged` hook fired on a `cd`
/// call [`register_all_with_cwd_firer`] instead. The mobile composition root
/// never registers these shell tools at all.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    register_all_with_cwd_firer(reg, ctx, None, None);
}

/// Register Bash, PowerShell, and REPL tools against `reg`, attaching an
/// optional `CwdChanged` hook firer AND an optional shared live-cwd cell to the
/// `BashTool`.
///
/// When `cwd_changed_firer` is `Some(..)`, a foreground `cd` inside a Bash call
/// that moves the persistent shell cwd fires the `CwdChanged` hook (1:1
/// `onCwdChangedForHooks`, `Shell.ts:409`). When `live_cwd` is `Some(..)`, that
/// cell becomes the `BashTool`'s persistent shell cwd, so a `cd` writes the SAME
/// live cwd (`getCwd()`/`setCwdState`) the file/search/LSP tools and the
/// orchestrator read (see [`BashTool::with_live_cwd`]). When both are `None`, the
/// `BashTool` is byte-identical to the plain [`register_all`] path. Only
/// `BashTool` is affected; PowerShell / REPL are unchanged.
pub fn register_all_with_cwd_firer(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    cwd_changed_firer: hooks::OptionalCwdChangedFirer,
    live_cwd: Option<tool_api::LiveCwdCell>,
) {
    use std::sync::Arc;
    let mut bash = BashTool::new(ctx.clone());
    if let Some(firer) = cwd_changed_firer {
        bash = bash.with_cwd_changed_firer(firer);
    }
    if let Some(cell) = live_cwd {
        bash = bash.with_live_cwd(cell);
    }
    reg.register_builtin(Arc::new(bash));
    reg.register_builtin(Arc::new(PowerShellTool::new(ctx.clone())));
    // REPL is experimental and default-OFF (claude-code 2.1.206 `kO()`): only
    // register it when enabled by `LINGXI_REPL` / the `tengu_slate_harbor` flag.
    if repl::is_repl_enabled() {
        reg.register_builtin(Arc::new(REPLTool::new(ctx)));
    }
}
