//! Top-level conversational orchestrator — drives the turn loop.
//!
//! `ConversationOrchestrator` is the single owner of an in-process AI
//! coding conversation:
//!
//! 1. Append the user prompt to `SessionState`.
//! 2. Call the model — batched (`messages_create`) or streaming, depending on
//!    the entry.
//! 3. Dispatch `tool_use` blocks through `ToolRegistry`, after the `PreToolUse`
//!    hook and the permission gate.
//! 4. Append the assistant message to the session.
//! 5. Loop until the turn ends or a limit stops it.
//!
//! There are THREE public entries over TWO loops — `run_turn`,
//! `run_turn_with_cancel` (batched) and `run_turn_streaming` (the one
//! `bridge-server` drives, so desktop and mobile go through it). The loops have
//! the same shape and share the steps they have in common — preparation, the
//! guards, the end-of-turn sequence (`conversation/drivers/`) — but they are
//! still two loops, and a branch added to one is dead on the other.
//! `docs/unified-conversation-driver-plan-2026-09-15.md`
//! §9.5 lists what is shared, what deliberately is not, and which test pins
//! each difference.
//!
//! See spec §2.2 (data flow diagram) and §4.2 (turn loop limits).
#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 15 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod api_error_copy;
pub mod bg_snapshot;
pub mod config;
pub mod conversation;
pub(crate) mod cost_lines;
pub mod cost_wiring;
pub mod cwd_changed_firer;
pub mod diagnostics;
pub mod end_conversation_tool;
pub mod error;
pub mod file_changed_firer;
pub mod handle_impl;
pub mod hook_attachment_sink;
pub mod hook_prompt_runner;
pub mod image_input;
pub mod loop_permission_classifier;
pub mod mcp_hook_dispatcher;
pub mod model;
pub mod prompt;
pub mod provider_adapter;
pub use lingxi_core::host::refusal_cascade;
pub mod turn_span;
pub use lingxi_core::host::refusal_notice;
pub mod resume;
mod scheduled_turn;
pub(crate) mod schema_validation;
pub mod sse;
pub mod stop_hook_snapshot;
pub(crate) mod streaming_executor;
pub mod streaming_loop;
pub mod structured_output;
pub mod task_completed_firer;
pub mod task_created_firer;
pub mod task_lifecycle_hook_firer;
pub mod task_notifications_provider;
pub mod teammate_idle_firer;
pub mod todo_reminder_tasks_provider;
pub mod token_budget;
pub mod tool_result_persistence;
pub mod transcript_paths;
pub mod turn_loop;
mod vision_model_call;

// test_support carries the HookExecutor / PermissionGate trait definitions
// that ConversationOrchestrator's signature uses; it MUST be available in
// production builds (the plan's `#[cfg(any(test, feature = "test-support"))]`
// guard would hide those traits from production). M5-05 / M5-06 will move
// the traits to lingxi-permission / lingxi-hooks; until then they live in
// test_support unconditionally.
pub mod test_support;

/// Streaming-path test fixtures (`MockStreamingApiClient`, `scripted!`).
/// Compiled unconditionally — mirrors `test_support` (M5-02 made that
/// module unconditional; this one follows the same convention so
/// integration tests can use the fixtures without a feature flag).
pub mod test_support_stream;

pub use config::{
    sanitize_query_source, OrchestratorConfig, MAX_TURNS_DEFAULT, QUERY_SOURCE_REPL_MAIN_THREAD,
    QUERY_SOURCE_SDK,
};
pub use conversation::{
    AppAgentPromptProfile, ConversationOrchestrator, ConversationOutcome, OrchestratorApiClient,
    SessionMemoryHandle, StreamingApiClient, TurnOutcome,
};
pub use conversation::{QueuedPromptInput, ScheduledLoopFire};
pub use cwd_changed_firer::OrchestratorCwdChangedFirer;
pub use error::OrchestratorError;
pub use file_changed_firer::OrchestratorFileChangedFirer;
pub use hook_attachment_sink::JsonlHookAttachmentSink;
pub use hook_prompt_runner::ApiClientHookPromptRunner;
pub use mcp_hook_dispatcher::OrchestratorHookDispatcher;
pub use prompt::{
    assemble_system_prompt, FileTree, FileTreeEntry, GitStatus, MemoryFile, SystemPromptContext,
};
pub use provider_adapter::ProviderApiAdapter;
pub use resume::{
    client_state_tool_results_from_messages, deferred_tool_replays_from_messages,
    prompt_snapshot_from_messages, replay_deferred_tools_after_resume, replay_session_state,
    runtime_metadata_from_messages, state_from_messages, ReplayedSession, ResumeError,
    ResumeRuntimeMetadata, CLIENT_STATE_TOOLS,
};
pub use stop_hook_snapshot::{
    build_background_tasks, build_session_crons, CronSnapshotInput, StopHookSnapshotProvider,
};
pub use task_completed_firer::OrchestratorTaskCompletedFirer;
pub use task_created_firer::OrchestratorTaskCreatedFirer;
pub use task_lifecycle_hook_firer::OrchestratorTaskLifecycleHookFirer;
pub use task_notifications_provider::RegistryTaskNotifications;
pub use teammate_idle_firer::OrchestratorTeammateIdleFirer;
pub use todo_reminder_tasks_provider::TodoStoreReminderTasks;
