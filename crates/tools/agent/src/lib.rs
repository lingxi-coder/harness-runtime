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
pub use agent::AgentTool;

pub mod classifier_handoff;

#[cfg(any(test, feature = "agent-test-support"))]
pub mod agent_test_support;

/// Register the agent (subagent dispatch) tools against `reg`.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    register_with_fusion(reg, ctx, None);
}

/// Register `AgentTool`, optionally injecting a Fusion executor.
pub fn register_with_fusion(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    fusion: Option<std::sync::Arc<dyn lingxi_core::host::FusionExecutor>>,
) {
    register_with_fusion_and_recorder(reg, ctx, fusion, None);
}

/// Register `AgentTool` with an optional common Fusion terminal recorder.
pub fn register_with_fusion_and_recorder(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    fusion: Option<std::sync::Arc<dyn lingxi_core::host::FusionExecutor>>,
    recorder: Option<std::sync::Arc<dyn lingxi_core::host::FusionRunRecorder>>,
) {
    register_with_fusion_and_recorder_factory(reg, ctx, fusion, recorder, None);
}

/// Register `AgentTool` with both a compatibility recorder and a pure
/// per-session recorder factory.
pub fn register_with_fusion_and_recorder_factory(
    reg: &mut tool_api::ToolRegistry,
    ctx: tool_api::BuiltinToolContext,
    fusion: Option<std::sync::Arc<dyn lingxi_core::host::FusionExecutor>>,
    recorder: Option<std::sync::Arc<dyn lingxi_core::host::FusionRunRecorder>>,
    factory: Option<std::sync::Arc<dyn lingxi_core::host::FusionRunRecorderFactory>>,
) {
    use std::sync::Arc;
    let mut tool = AgentTool::new(ctx);
    if let Some(executor) = fusion {
        tool = tool.with_fusion(executor);
    }
    if let Some(recorder) = recorder {
        tool = tool.with_terminal_recorder(recorder);
    }
    if let Some(factory) = factory {
        tool = tool.with_terminal_recorder_factory(factory);
    }
    reg.register_builtin(Arc::new(tool));
}
