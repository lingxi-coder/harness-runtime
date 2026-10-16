//! Agent tool: AgentTool (subagent dispatch). Extracted in M8-P7.
//! Desktop-only. Dispatches via the SubagentSpawner carried in
//! BuiltinToolContext (a `traits` seam), so no dep on the `agent` engine crate.
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
pub mod agent;
pub use agent::{AgentFusionServices, AgentTool};

pub mod classifier_handoff;

#[cfg(any(test, feature = "agent-test-support"))]
pub mod agent_test_support;

/// Register the agent (subagent dispatch) tools against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    register_with_fusion_services(reg, ctx, None);
}

/// Register `AgentTool` with no Fusion capability or one complete current
/// Fusion executor/recorder-factory pair.
pub fn register_with_fusion_services(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    fusion: Option<AgentFusionServices>,
) {
    use std::sync::Arc;
    let mut tool = AgentTool::new(ctx);
    if let Some(services) = fusion {
        tool = tool.with_fusion_services(services);
    }
    reg.register_builtin(Arc::new(tool));
}
