//! Agent and subagent runtime for `LingXi` Core.
//!
//! Owns the [`pool::StateMachinePool`] (effect-delegated, sibling-slot model
//! rather than nested state machines), the cross-system
//! [`context::SubagentContext`] builder, [`multi_dispatch::MultiAgentDispatcher`]
//! for parallel spawn, the [`color_manager::AgentColorManager`], the
//! [`tool_resolver::AgentToolResolver`], and the worktree degradation policy.
//!
//! See spec §10 for the architecture overview.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 5 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 8 item(s) rustc could
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

/// Streaming assembly, re-exported from `llm-runtime`.
///
/// This module used to OWN the accumulator. It now lives in `llm-runtime`,
/// beside the `HistoryEvent`/`HistoryResponse` it is defined in terms of, so other
/// stream consumers (the mobile local-app generator) reuse the same assembly
/// instead of growing a second one. The alias keeps every call site here
/// unchanged.
mod accumulator {
    pub(crate) use llm_runtime::stream_accumulator::{
        accumulate_stream_salvaging, response_to_stream_events,
    };
}
pub mod agent_mcp_tools;
pub mod api;
pub mod builtins;
pub mod catalog;
pub mod color_manager;
pub mod context;
pub mod definition;
pub mod display;
pub mod fork;
pub mod handle;
pub mod hooks_trust;
pub mod mcp_servers;
pub mod model_resolution;
pub mod multi_dispatch;
pub mod observer;
pub mod observer_delivery;
pub mod observer_text;
pub mod permission_mode;
pub mod pool;
pub mod runner;
pub mod tool_resolver;
pub mod transcript;
pub mod worktree_policy;

pub use api::{NearLimitCheckpointRequest, SubagentApiClient};
pub use builtins::{
    builtin_agent_definitions, fork_agent_definition, fusion_analyst_definition,
    fusion_panel_definition,
};
pub use catalog::{
    load_agents_from_dirs, parse_agent_from_json, parse_agent_markdown,
    parse_agents_from_flag_json, parse_agents_from_flag_json_checked, parse_agents_from_json,
    AgentLoadError,
};
pub use color_manager::AgentColorManager;
pub use context::SubagentContext;
pub use definition::*;
pub use display::AgentDisplay;
pub use handle::{
    agent_listing_entries, tools_denied_agent_types, tools_description,
    with_transcript_subdir_override, workflow_transcript_subdir_override, DefaultModelSelection,
    PoolSubagentSpawner, ProviderFirstPartyResolver, RuntimeLink, StreamingSubagentSpawner,
};
// `agent_listing_delta` shared surface: the ONE `formatAgentLine` and the
// `shouldInjectAgentListInMessages` gate live in the leaf `platform-api` crate (so
// `tool-agent` can reach them without depending on this engine crate); re-export
// them here under the `agent::` path the orchestrator + callers use.
pub use lingxi_core::host::subagent_spawn::{
    format_agent_line, should_inject_agent_list_in_messages,
};
// Fork-subagent helpers live in the leaf `platform-api` crate (reachable by both
// `tool-agent` and `agent`); re-export under `agent::` for ergonomic access.
pub use lingxi_core::host::fork_subagent::{
    build_child_message, build_forked_messages, build_worktree_notice, is_fork_subagent_enabled,
    is_in_fork_child, FORK_SUBAGENT_TYPE,
};
pub use mcp_servers::agent_mcp_specs_to_scoped_configs;
pub use model_resolution::resolve_agent_model;
pub use observer::{
    propagation_for_spawn, validate_observer_graph, ObserverPropagation, ObserverValidationError,
    DEFAULT_OBSERVER_FANOUT_DEPTH,
};
pub use tool_resolver::{augment_teammate_tool_policy, resolve_subagent_tools};
// Re-export `ToolRegistry` (from `tool_api`, an existing `agent` dep) so the
// `tasks` in-process-teammate handler can hold one for per-spawn tool resolution
// without widening its own dep graph.
pub use tool_api::ToolRegistry;
// Re-export `PermissionMode` (lives in the `permission` crate, which `agent`
// already depends on) so the `tasks` crate can reference `agent::PermissionMode`
// for `resolve_agent_model`'s seam without widening its own dep graph.
pub use multi_dispatch::{MultiAgentDispatcher, MultiAgentSpawnSpec};
pub use permission::PermissionMode;
pub use pool::{StateMachinePool, StateMachineSlot};
pub use runner::SubagentEvent;
pub use tool_resolver::AgentToolResolver;
pub use worktree_policy::create_worktree_or_degrade;
