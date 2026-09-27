//! Hook system — 28-event observability + intervention plane.
//!
//! Hooks fire around tool dispatch, session lifecycle, permission requests,
//! and many other engine touchpoints. Each registered hook can observe an
//! event and optionally return a [`HookResponse`] that blocks, approves, or
//! mutates the in-flight action.
//!
//! M1.4 ships the data model and an executor scaffold: the 28-variant event
//! enum, hook definition / source taxonomy, response aggregation, the
//! [`HookExecutorImpl`], and the [`SsrfGuard`]. The full Command / HTTP /
//! Agent executor bodies and the in-flight tracker land in Plan 09.
//!
//! See spec §9 (Hook System) and D17 (Runtime boundary — no direct tokio).

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 61 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

mod agent_executor;
pub mod async_registry;
pub mod attachment;
pub mod builtin;
pub mod cwd_changed_firer;
pub mod definition;
pub mod events;
pub mod executor;
pub mod file_changed_firer;
pub mod function_hook;
pub mod hook_payload;
mod http_executor;
pub mod loader;
pub mod matcher;
/// Name-addressed MCP hook invocation seam.
pub mod mcp_invoker;
pub mod prompt_executor;
pub mod registry;
pub mod response;
/// Strict parse/validate/serialize helpers for a settings-layer `hooks` object.
pub mod settings_layer;
pub mod ssrf_guard;
pub mod task_completed_firer;
pub mod task_created_firer;
pub mod teammate_idle_firer;
pub mod terminal_seq;
pub mod user_config;
pub mod watcher_rebinder;

pub use async_registry::AsyncHookRegistry;
pub use attachment::{
    additional_context_attachment, blocking_error_attachment, blocking_error_prose,
    cancelled_attachment, deferred_tool_attachment, error_during_execution_attachment,
    non_blocking_error_attachment, stopped_continuation_attachment, success_attachment,
    system_message_attachment, BlockingError, CancellationTimeout, HookAttachmentIdentity,
    HookAttachmentSink,
};
pub use cwd_changed_firer::{CwdChangedFire, CwdChangedFirer, OptionalCwdChangedFirer};
pub use definition::{HookCondition, HookDefinition, HookExecutor, HookShell, HookSource};
pub use events::{HookEvent, HookEventType, HookProgressEvent};
pub use executor::{
    default_hook_shell, powershell_base_args, powershell_env_token_rewrite,
    powershell_missing_error, references_bare_project_dir_var, resolve_powershell_executable,
    BuiltinHookHandler, HookExecutorImpl, HOOK_AGENT_TIMEOUT_MS, HOOK_COMMAND_TIMEOUT_MS,
    HOOK_HTTP_TIMEOUT_MS,
};
pub use file_changed_firer::{FileChangedFire, FileChangedFirer, OptionalFileChangedFirer};
pub use hook_payload::{
    parse_response, validation_hint, HookBackgroundTask, HookEventEnvelope, HookEventNamePost,
    HookEventNamePostModelSwitch, HookEventNamePre, HookEventNamePreModelSwitch,
    HookResponseParseError, HookSessionCron, PostModelSwitchPayload, PostToolUsePayload,
    PreModelSwitchPayload, PreToolUsePayload,
};
pub use loader::{
    parse_hooks_from_settings_json, parse_hooks_from_settings_json_gated, HookPolicyGate,
};
pub use matcher::{get_legacy_tool_names, matches_pattern, normalize_legacy_tool_name};
pub use mcp_invoker::{HookMcpInvocation, HookMcpInvocationResult, HookMcpInvoker};
pub use prompt_executor::{
    HookPromptRunner, PromptHookError, PromptHookRequest, PromptHookTranscript,
    HOOK_PROMPT_TIMEOUT_MS,
};
pub use registry::{HookContext, HookRegistry, HookSourceReplaceResult};
pub use response::{
    truncate_utf16, AggregateHookResult, ClassifierHostContext, ElicitationHookResponse,
    HookDecision, HookOutcome, HookResponse, HookResult, PairedRewrite, PermissionRequestResult,
    CLASSIFIER_CONTEXT_CAP_UTF16,
};
pub use settings_layer::{
    event_name_for_hook, file_changed_matchers_from_hooks, parse_hook_layer_strict,
    parse_hook_layer_value_strict, serialize_hook_layer, HookLayerDocument, HookLayerError,
    HookValidationIssue, ParsedHookLayer,
};
pub use ssrf_guard::{DnsResolver, IpRange, SsrfError, SsrfGuard};
pub use task_completed_firer::{OptionalTaskCompletedFirer, TaskCompletedFire, TaskCompletedFirer};
pub use task_created_firer::{OptionalTaskCreatedFirer, TaskCreatedFire, TaskCreatedFirer};
pub use teammate_idle_firer::{
    OptionalTeammateIdleFirer, TeammateIdleFire, TeammateIdleFirer, TeammateIdleOutcome,
};
pub use watcher_rebinder::{OptionalWatcherRebinder, WatcherRebinder};
