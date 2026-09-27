//! Skill execution tool. Extracted in M8-P7. Cross-platform.
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
pub mod fork;
pub mod model_override;
// Shared prompt `!`cmd`` shell-expansion provider (host runner + policy-backed
// gate). Relocated here from `tool-api` (parity 2.1.207 §8.1): it bridges
// `tool_api::BuiltinToolContext` with `command_api`'s shell-expansion traits, so
// it must live in a non-API crate that may depend on BOTH — `tool-skill`
// already does, and is the original home of the runner. The dispatcher / TUI /
// skill all inject the provider built here.
pub mod prompt_shell;
pub mod skill;
pub use prompt_shell::{
    build_prompt_shell_provider, resolve_shell_path, PromptShellExpansionProvider,
    PromptShellRunner,
};
pub use skill::SkillTool;
/// Register the skill-management tool against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(SkillTool::new(ctx)));
}
