//! Tool API — the abstract surface every tool implementation and every
//! engine-side consumer depends on: the [`Tool`] trait, [`ToolUseContext`],
//! [`ToolRegistry`], progress channel, and content-replacement state.
//!
//! Extracted from the `tools` monolith in M8-P3 so engine crates can depend
//! on the trait surface without pulling in the 41 builtin implementations.
//! Trait semantics, error variants, and field shapes are byte-equivalent to
//! the pre-split `tools` crate. The §5 `ToolCtx` reshape (full Platform
//! handles, split `error`/`schema` modules) is deferred to P4.

#![forbid(unsafe_code)]
#![allow(
    clippy::module_name_repetitions,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

pub mod artifact_gate;
pub mod ask_user_question;
pub mod bash_precommit;
pub mod bash_runner;
pub mod builtin_context;
pub mod content_replacement;
pub mod context;
pub mod defer;
pub mod hosted_search;
pub mod model_prompt_gate;
pub mod native_schema;
pub mod nested_memory_triggers;
pub mod output_schema;
pub mod progress;
pub mod read_file_state;
pub mod registry;
pub mod sandbox_runner;
pub mod session_cwd;
pub mod todo_tools_gate;
pub mod tool_invoker_impl;
pub mod tool_result_media;
pub mod tool_search_view;
pub mod tool_trait;
pub mod util;
pub mod wire;
pub mod worktree_session;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use builtin_context::{
    AndroidGitSecret, AndroidGitToolCtx, AndroidShellToolCtx, BashEditDiffSetup,
    BuiltinToolContext, GitCredentialProvider, LiveCwdCell, MainLoopModelProfileProvider,
    MobileGitSecret, MobileGitToolCtx, MobileShellToolCtx, TaskLifecycleHookFirer,
};
pub use content_replacement::ContentReplacementState;
pub use context::{ToolUseContext, ToolUseOptions};
pub use defer::{
    mode_from_env, mode_from_values, DeferralState, ToolSearchMode, ENTER_WORKTREE_TOOL_NAME,
};
pub use hosted_search::{
    HostedSearchError, HostedSearchOutput, HostedSearchRequest, HostedWebSearchClient,
    McpTokenCounter,
};
pub use model_prompt_gate::dh_simple_system_prompt;
pub use progress::{progress_channel, ToolProgress, ToolProgressReceiver, ToolProgressSender};
pub use read_file_state::{ReadFileEntry, ReadFileStateMap};
pub use registry::ToolRegistry;
pub use sandbox_runner::{default_sandbox_runner, LegacyWrapRunner, SandboxRunner};
pub use session_cwd::SessionCwd;
pub use todo_tools_gate::todo_tools_enabled;
pub use tool_invoker_impl::RegistryToolInvoker;
pub use tool_search_view::{
    SharedToolSearchView, StaticRegistryView, ToolRegistryView, ToolSearchEntry,
};
pub use tool_trait::*;
pub use worktree_session::{
    new_worktree_session_cell, WorktreeSession, WorktreeSessionCell, WorktreeStatePersister,
};

/// Shared current SendMessage schema, prose, and input coercion.
pub mod send_message_contract;

// Native sandbox adapters implement this without depending on a concrete HTTP client.
pub use lingxi_core::host::http::{MonitorSocketIo, MonitorWebSocketProxy};
