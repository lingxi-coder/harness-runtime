//! Meta tools: Config, ToolSearch. Extracted in M8-P7. Cross-platform.
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
pub mod config;
pub mod tool_search;
pub use config::ConfigTool;
pub use tool_search::ToolSearchTool;
/// Register the meta tools (Config, ToolSearch) against `reg`.
///
/// `ToolSearch` is wired to the registry's LIVE deferred-tool view cell and its
/// shared [`DeferralState`](tool_api::DeferralState) (env-derived mode). The
/// composition root must call `reg.refresh_tool_search_view()` once the registry
/// is fully assembled (including MCP tools) to populate the view; the shared
/// state is disabled by default, so an un-refreshed / non-tool-search session is
/// byte-identical to the pre-pipeline build.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    reg.register_builtin(Arc::new(ConfigTool::new(ctx.clone())));
    // Install the env-derived deferral state on the registry and share the SAME
    // `Arc` with the ToolSearch tool, so the wire serializer and the search
    // consumer observe one loaded-set.
    let deferral = Arc::new(tool_api::DeferralState::from_env());
    reg.set_deferral(deferral.clone());
    let view: Arc<dyn tool_api::ToolRegistryView> = reg.tool_search_view();
    reg.register_builtin(Arc::new(ToolSearchTool::with_view_and_deferral(
        ctx, view, deferral,
    )));
}
