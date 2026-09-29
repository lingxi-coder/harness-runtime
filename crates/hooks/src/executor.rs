//! Hook executor.
//!
//! M1.4 shipped [`BuiltinHookHandler`] and the [`HookExecutorImpl`] with the
//! `Builtin` arm fully wired and the `Http` / `Command` / `Agent` arms
//! stubbed. M5-06 filled in the HTTP and Agent arms (delegating to
//! [`crate::http_executor::HttpExecutor`] / [`crate::agent_executor::AgentExecutor`]).
//! The Command arm is now wired too: it feeds the serialized hook payload to a
//! child process on stdin via the [`ProcessRunner`] + [`Sandbox`] traits and
//! maps the process exit code / stdout to a [`HookResult`] per the claude-code
//! command-hook contract (`claude-code/src/utils/hooks.ts`).

use crate::agent_executor::{AgentExecutionSignal, AgentExecutor};
use crate::async_registry::{
    AsyncHookRegistry, HookCompletion, HookWork, DEFAULT_ASYNC_HOOK_TIMEOUT_MS,
};
use crate::attachment::{self, CancellationTimeout, HookAttachmentIdentity, HookAttachmentSink};
use crate::definition::{HookDefinition, HookExecutor};
use crate::events::HookEvent;
use crate::hook_payload::{
    parse_response, ConfigChangePayload, CwdChangedPayload, DirectoryAddedPayload,
    ElicitationPayload, ElicitationResultPayload, FileChangedPayload, HookEventNameConfigChange,
    HookEventNameCwdChanged, HookEventNameDirectoryAdded, HookEventNameElicitation,
    HookEventNameElicitationResult, HookEventNameFileChanged, HookEventNameInstructionsLoaded,
    HookEventNameMessageDisplay, HookEventNameNotification, HookEventNamePermissionDenied,
    HookEventNamePermissionRequest, HookEventNamePost, HookEventNamePostCompact,
    HookEventNamePostModelSwitch, HookEventNamePostToolBatch, HookEventNamePostToolUseFailure,
    HookEventNamePre, HookEventNamePreCompact, HookEventNamePreModelSwitch,
    HookEventNameSessionEnd, HookEventNameSessionStart, HookEventNameSetup, HookEventNameStop,
    HookEventNameStopFailure, HookEventNameSubagentStart, HookEventNameSubagentStop,
    HookEventNameTaskCompleted, HookEventNameTaskCreated, HookEventNameTeammateIdle,
    HookEventNameUserPromptExpansion, HookEventNameUserPromptSubmit, HookEventNameWorktreeCreate,
    HookEventNameWorktreeRemove, InstructionsLoadedPayload, MessageDisplayPayload,
    NotificationPayload, PermissionDeniedPayload, PermissionRequestPayload, PostCompactPayload,
    PostModelSwitchPayload, PostToolBatchPayload, PostToolUseFailurePayload, PostToolUsePayload,
    PreCompactPayload, PreModelSwitchPayload, PreToolUsePayload, SessionEndPayload,
    SessionStartPayload, SetupPayload, StopFailurePayload, StopPayload, SubagentStartPayload,
    SubagentStopPayload, TaskCompletedPayload, TaskCreatedPayload, TeammateIdlePayload,
    UserPromptExpansionPayload, UserPromptSubmitPayload, WorktreeCreatePayload,
    WorktreeRemovePayload,
};
use crate::http_executor::{HttpExecutionSignal, HttpExecutor, HttpHookPolicy};
use crate::mcp_invoker::{HookMcpInvocation, HookMcpInvocationResult, HookMcpInvoker};
use crate::prompt_executor::{
    HookPromptRunner, PromptExecutionSignal, PromptExecutor, HOOK_PROMPT_TIMEOUT_MS,
};
use crate::registry::{HookContext, HookRegistry};
use crate::response::{
    AggregateHookResult, HookDecision, HookOutcome, HookResponse, HookResult,
    PermissionRequestResult,
};
use crate::ssrf_guard::SsrfGuard;
use async_trait::async_trait;
use mobile_linux_api::ProcessError;
use platform_api::subagent_spawn::SubagentSpawner;
use platform_api::{
    HttpTransport, OutputStream, ProcessCommand, ProcessRunner, RuntimeSpawner, Sandbox,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Default HTTP hook timeout (10 minutes — matches
/// `claude-code/src/utils/hooks/execHttpHook.ts:12` `DEFAULT_HTTP_HOOK_TIMEOUT_MS`).
pub const HOOK_HTTP_TIMEOUT_MS: u64 = 600_000;

/// Default command hook timeout (10 minutes — matches
/// `claude-code/src/utils/hooks.ts:166` `TOOL_HOOK_EXECUTION_TIMEOUT_MS`).
pub const HOOK_COMMAND_TIMEOUT_MS: u64 = 600_000;

/// Default agent hook timeout (60 seconds — matches
/// `claude-code/src/utils/hooks/execAgentHook.ts:75` fall-through default).
pub const HOOK_AGENT_TIMEOUT_MS: u64 = 60_000;

/// Backstop timeout for a function hook. The real cap is the sandbox's own
/// budget (`function_hook::Sandbox::budget`); this only bounds the surrounding
/// machinery so a wedged executor cannot outlive it.
pub const HOOK_FUNCTION_TIMEOUT_MS: u64 = 5_000;

/// Cap on the error text a failing function hook may put in the transcript.
///
/// The message is plugin-authored and unbounded; the attachment layer stores
/// `stderr` verbatim (`persist_large_hook_output` covers only stdout), so
/// without this a hook pushes megabytes into the transcript on every event.
const FUNCTION_HOOK_STDERR_CAP: usize = 4_096;

/// Process-env override for the `SessionEnd` hook *batch* shutdown deadline
/// (claude-code `Wqt`, BIN off 205715763:
/// `process.env.LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS`). When this parses as a
/// finite integer `> 0` it is used verbatim (NOT clamped — the binary returns it
/// directly, bypassing the floor/cap).
pub const SESSION_END_HOOKS_TIMEOUT_ENV: &str = "LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS";

/// Floor for the computed `SessionEnd` batch deadline (claude-code `nzn`, BIN off
/// 205765355: `nzn=1500`, exported as `SESSION_END_HOOK_TIMEOUT_MS_DEFAULT`).
/// `Wqt` returns `Math.max(nzn, Math.min(n, rym))`, so the batch deadline is
/// never shorter than this when the env override is absent. Also doubles as the
/// per-hook default timeout the `SessionEnd` runner passes (`lje` →
/// `cH({…, timeoutMs: nzn})`, BIN off 205706285).
pub const SESSION_END_HOOK_TIMEOUT_FLOOR_MS: u64 = 1_500;

/// Cap for the computed `SessionEnd` batch deadline (claude-code `rym`, BIN off
/// 205765364: `rym=60000`). `Wqt` returns `Math.max(nzn, Math.min(n, rym))`, so
/// the computed deadline is never longer than this when the env override is
/// absent.
pub const SESSION_END_HOOK_TIMEOUT_CAP_MS: u64 = 60_000;

/// Compute the `SessionEnd` hook *batch* shutdown deadline in milliseconds, faithful
/// to claude-code `Wqt` (BIN off 205715763):
///
/// ```text
/// function Wqt(){
///   let e=process.env.LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS,
///       t=e?parseInt(e,10):NaN;
///   if(Number.isFinite(t)&&t>0)return t;            // env override wins, UNCLAMPED
///   let n=0,r=uE()?[]:Kj()?.SessionEnd??[],
///       o=[...f5()?.SessionEnd??[],...r];
///   for(let s of o)for(let i of s.hooks)
///     if(i.timeout&&i.timeout*1000>n)n=i.timeout*1000; // max per-hook timeout (s→ms)
///   return Math.max(nzn,Math.min(n,rym))             // clamp to [1500, 60000]
/// }
/// ```
///
/// * `env_value` is the raw `LINGXI_SESSIONEND_HOOKS_TIMEOUT_MS` string (or
///   `None`). A value that parses (base-10, leading-digit, JS `parseInt`-style)
///   to a finite `> 0` integer is returned VERBATIM — the env override bypasses
///   the floor/cap, exactly as the binary does.
/// * Otherwise the deadline is `max(FLOOR, min(max_per_hook_ms, CAP))`, where
///   `max_per_hook_ms` is the largest declared per-hook timeout across the
///   matched `SessionEnd` hooks (each hook's `Duration` in ms; an absent / zero
///   timeout contributes `0`). With no per-hook timeouts this collapses to the
///   1500 ms floor.
#[must_use]
pub fn session_end_batch_timeout_ms(env_value: Option<&str>, max_per_hook_ms: u64) -> u64 {
    if let Some(raw) = env_value {
        if let Some(parsed) = parse_int_base10(raw) {
            // `Number.isFinite(t) && t > 0` — `parseInt` of a non-numeric prefix
            // yields NaN (→ `None` here, falls through); a non-positive value is
            // rejected and also falls through to the computed clamp.
            if parsed > 0 {
                // u64 already bounds finiteness; a positive value is returned
                // unclamped, mirroring `return t`.
                #[allow(clippy::cast_sign_loss)]
                return parsed as u64;
            }
        }
    }
    SESSION_END_HOOK_TIMEOUT_FLOOR_MS.max(max_per_hook_ms.min(SESSION_END_HOOK_TIMEOUT_CAP_MS))
}

/// Largest declared per-hook timeout (in milliseconds) across a matched hook set
/// — the `n` accumulator in `Wqt` (`if(i.timeout&&i.timeout*1000>n)n=i.timeout*1000`).
/// A hook with no timeout (`None`) or a zero timeout contributes `0`.
fn max_per_hook_timeout_ms(hooks: &[HookDefinition]) -> u64 {
    hooks
        .iter()
        .filter_map(|h| h.timeout)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .max()
        .unwrap_or(0)
}

/// JS `parseInt(s, 10)`-style parse: skip leading ASCII whitespace, accept an
/// optional sign, then consume the leading run of decimal digits and ignore any
/// trailing non-digit suffix (so `"3000abc"` → `3000`, `"abc"` → `NaN`/`None`).
/// Returns `None` for no leading digits (the `NaN` case) so the caller falls
/// through to the computed clamp.
fn parse_int_base10(s: &str) -> Option<i64> {
    let bytes = s.trim_start().as_bytes();
    let mut idx = 0;
    let mut negative = false;
    if let Some(&first) = bytes.first() {
        if first == b'+' || first == b'-' {
            negative = first == b'-';
            idx = 1;
        }
    }
    let digits_start = idx;
    let mut value: i64 = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(bytes[idx] - b'0'));
        idx += 1;
    }
    if idx == digits_start {
        // No digits consumed → `parseInt` returns NaN.
        return None;
    }
    Some(if negative { -value } else { value })
}

/// In-process Rust handler for [`HookExecutor::Builtin`] hooks.
///
/// Implementations are registered with [`HookExecutorImpl::register_builtin`]
/// and looked up by `id` when an event fires.
#[async_trait]
pub trait BuiltinHookHandler: Send + Sync {
    /// Handle the event and produce a [`HookResult`]. Implementations should
    /// avoid blocking work — long operations should be deferred to the
    /// background async registry.
    async fn handle(&self, event: &HookEvent, ctx: &HookContext) -> HookResult;
    /// Stable handler identifier matching the `handler_id` carried by
    /// [`HookExecutor::Builtin`] definitions.
    fn id(&self) -> &str;
}

/// Default executor — dispatches each matched hook to the appropriate
/// runner kind and aggregates their responses.
///
/// The `http` transport is shared with the rest of the engine so requests
/// flow through the same retry / telemetry plumbing.
type AgentPromptTranscripts =
    HashMap<(protocol::SessionId, protocol::AgentId), crate::PromptHookTranscript>;

pub struct HookExecutorImpl {
    agent_prompt_transcripts: std::sync::Mutex<AgentPromptTranscripts>,
    registry: Arc<RwLock<HookRegistry>>,
    http: Arc<dyn HttpTransport>,
    /// Background spawner. The Command arm's child runs on the `ProcessRunner`;
    /// this drives the `hook_progress` poll task (SH-07) and is snapshotted onto
    /// every [`Dispatcher`].
    runtime: Arc<dyn RuntimeSpawner>,
    builtin_handlers: HashMap<String, Arc<dyn BuiltinHookHandler>>,
    ssrf_guard: SsrfGuard,
    /// Optional subagent spawner — attached via [`Self::with_agent_spawner`].
    /// When `None`, the `Agent` arm returns `HookOutcome::Error` with
    /// `stderr: "Hook {id} failed: agent executor not wired"`. M5-06.
    agent_spawner: Option<Arc<dyn SubagentSpawner>>,
    /// Optional single-turn LLM runner — attached via
    /// [`Self::with_prompt_runner`]. When `None`, the `Prompt` arm returns a
    /// structured "not wired" no-op (never blocks). The orchestrator wires this
    /// over its existing one-shot `messages_create` call to keep the hooks
    /// crate decoupled from the api-client.
    prompt_runner: Option<Arc<dyn HookPromptRunner>>,
    /// Optional process runner — attached via [`Self::with_process_runner`].
    /// When `None`, the `Command` arm returns `HookOutcome::Error` with
    /// `stderr: "Hook {id} failed: command executor not wired"`.
    process: Option<Arc<dyn ProcessRunner>>,
    /// Optional sandbox — required alongside `process` to mint the
    /// [`platform_api::SandboxedCommand`] the runner accepts. Attached via
    /// [`Self::with_process_runner`].
    sandbox: Option<Arc<dyn Sandbox>>,
    /// Optional name-addressed MCP invoker for `mcp_tool` hooks.
    mcp_invoker: Option<Arc<dyn HookMcpInvoker>>,
    /// `YYe()` — whether this is a confined eval-harness run, resolved ONCE at
    /// construction rather than re-read inside the merge fold.
    ///
    /// Reading `CLAUDE_CODE_EVAL_CONFINED` at the gate made the flag process
    /// global: `cargo test` runs a binary's tests on parallel threads in one
    /// process, so a test that armed the variable silently suppressed hook
    /// allows for every other test in flight. Same fix, same reason as
    /// `PermissionPolicy::from_rules` — read the environment at the edge and
    /// pass the answer down.
    eval_confined: bool,
    /// Per-executor override for the SessionEnd batch deadline, in place of
    /// reading [`SESSION_END_HOOKS_TIMEOUT_ENV`] at dispatch. `None` reads the
    /// environment as before.
    session_end_timeout_override: Option<String>,
    /// Optional background registry for non-blocking (`blocking == false`)
    /// hooks (B5). Attached via [`Self::with_async_registry`]. When `None`, a
    /// non-blocking hook falls back to running synchronously (so its result is
    /// never silently dropped) — the engine simply gains no backgrounding.
    async_registry: Option<Arc<AsyncHookRegistry>>,
    /// #41: runner-head gate (`h$`, BIN off 195521423 =
    /// `Hn("policySettings")?.disableAllHooks===!0`). When `true`, EVERY
    /// `execute*` path skips all matched hooks and logs
    /// `Skipping hooks for {event[:matcher]} due to 'disableAllHooks' managed
    /// setting` — mirroring the binary's runner head `if(h$())return …,[]` ahead
    /// of the workspace-trust gate. Sourced from resolved managed/policy settings
    /// at the engine composition root and attached via
    /// [`Self::with_policy_disable_all_hooks`]; defaults `false` (the no-managed
    /// -policy path) so it is behavior-neutral until a policy is wired.
    policy_disable_all_hooks: bool,
    /// Optional output stream observer for `--include-hook-events` frames.
    ///
    /// When set, `execute` and `execute_session_end` emit
    /// `hook_started` / `hook_response` frames through this sink BEFORE and
    /// AFTER each blocking hook dispatches. Non-blocking (background) hooks
    /// are excluded — their completion is asynchronous and cannot be paired
    /// with a reliable "before" frame. Attached via
    /// [`Self::with_hook_observer`]; `None` by default (no-op, zero cost).
    hook_observer: Option<Arc<dyn OutputStream>>,
    /// H-BIN-12: CC 2.1.207 HTTP-hook security policy sourced from the
    /// `allowedHttpHookUrls` / `httpHookAllowedEnvVars` settings. Attached via
    /// [`Self::with_http_hook_policy`] at the composition root; the default
    /// (both `None` = no restriction) is behavior-neutral, so an engine that
    /// declares neither setting dispatches HTTP hooks exactly as before.
    http_hook_policy: HttpHookPolicy,
    /// Optional sink for the per-hook-run transcript `attachment` records
    /// (`hook_success` / `hook_non_blocking_error` / `hook_cancelled`).
    /// Attached via [`Self::with_attachment_sink`] at the composition root;
    /// `None` by default. Synchronous records are still carried on
    /// [`AggregateHookResult::hook_attachments`]; genuinely asynchronous
    /// completions require this sink because their aggregate has already been
    /// returned.
    attachment_sink: Option<Arc<dyn HookAttachmentSink>>,
}

impl HookExecutorImpl {
    /// Build a new executor backed by the supplied registry, HTTP transport,
    /// and runtime spawner.
    ///
    /// The Agent-arm is uncwired by default. Call [`Self::with_agent_spawner`]
    /// post-construction to attach a `SubagentSpawner` for Agent hooks.
    #[must_use]
    pub fn new(
        registry: Arc<RwLock<HookRegistry>>,
        http: Arc<dyn HttpTransport>,
        runtime: Arc<dyn RuntimeSpawner>,
    ) -> Self {
        Self {
            agent_prompt_transcripts: std::sync::Mutex::new(HashMap::new()),
            registry,
            http,
            runtime,
            builtin_handlers: HashMap::new(),
            ssrf_guard: SsrfGuard::with_defaults(),
            agent_spawner: None,
            prompt_runner: None,
            process: None,
            sandbox: None,
            mcp_invoker: None,
            eval_confined: eval_confined_session(),
            session_end_timeout_override: None,
            async_registry: None,
            policy_disable_all_hooks: false,
            hook_observer: None,
            http_hook_policy: HttpHookPolicy::default(),
            attachment_sink: None,
        }
    }

    /// Attach the transcript sink for per-hook-run `attachment` records.
    ///
    /// claude-code persists exactly ONE `attachment` transcript line per hook
    /// run; the sink is the engine's transcript writer. Default `None` is
    /// behavior-neutral for synchronous runs, which remain available through
    /// [`AggregateHookResult::hook_attachments`]. A detached completion cannot
    /// be added to an aggregate that has already returned, so wiring a sink is
    /// required to retain asynchronous run records.
    #[must_use]
    pub fn with_attachment_sink(mut self, sink: Arc<dyn HookAttachmentSink>) -> Self {
        self.attachment_sink = Some(sink);
        self
    }

    /// Attach the #41 runner-head gate (`h$`). When `disable_all_hooks` is
    /// `true`, every `execute*` path skips all matched hooks and logs the
    /// `'disableAllHooks' managed setting` skip line — mirroring the binary's
    /// `if(h$())return …,[]` runner head. Sourced at the composition root from
    /// the resolved `policySettings.disableAllHooks` managed setting (see
    /// [`crate::loader::HookPolicyGate`]); the default (`false`) is the
    /// no-managed-policy path and is behavior-neutral.
    #[must_use]
    pub fn with_policy_disable_all_hooks(mut self, disable_all_hooks: bool) -> Self {
        self.policy_disable_all_hooks = disable_all_hooks;
        self
    }

    /// Override the confined-eval-session flag (`YYe()`).
    ///
    /// The default comes from `CLAUDE_CODE_EVAL_CONFINED` at construction. This
    /// exists so a test can exercise the confined fold WITHOUT mutating a
    /// process-global that its neighbours are reading concurrently.
    #[must_use]
    pub fn with_eval_confined(mut self, eval_confined: bool) -> Self {
        self.eval_confined = eval_confined;
        self
    }

    /// Override the SessionEnd batch deadline instead of setting
    /// [`SESSION_END_HOOKS_TIMEOUT_ENV`].
    ///
    /// Same reason as [`Self::with_eval_confined`]: the env var is a process
    /// global, so a test that mutates it is visible to every other test
    /// dispatching SessionEnd on a neighbouring thread.
    #[must_use]
    pub fn with_session_end_timeout_ms(mut self, timeout_ms: &str) -> Self {
        self.session_end_timeout_override = Some(timeout_ms.to_string());
        self
    }

    /// Attach the H-BIN-12 HTTP-hook security policy (CC 2.1.207
    /// `allowedHttpHookUrls` / `httpHookAllowedEnvVars`, byte-faithful `PFy()`).
    ///
    /// - `allowed_urls`: `None` ⇒ all URLs allowed (default); `Some(empty)` ⇒
    ///   block ALL HTTP hooks; `Some(patterns)` ⇒ the hook URL must match ≥1
    ///   wildcard pattern (CC `NBr`) or the request is blocked before dispatch
    ///   with the byte-exact `HTTP hook blocked: …` warn line.
    /// - `allowed_env_vars`: `None` ⇒ the per-hook `allowedEnvVars` is used
    ///   as-is (default); `Some(list)` ⇒ each hook's effective allowlist is its
    ///   own `allowedEnvVars` intersected with this global list.
    ///
    /// Both `None` (the default) is behavior-neutral. Sourced at the composition
    /// root from the merged settings; primitives (not the private policy type)
    /// so external callers need no crate-internal imports.
    #[must_use]
    pub fn with_http_hook_policy(
        mut self,
        allowed_urls: Option<Vec<String>>,
        allowed_env_vars: Option<Vec<String>>,
    ) -> Self {
        self.http_hook_policy = HttpHookPolicy {
            allowed_urls,
            allowed_env_vars,
        };
        self
    }

    /// Attach an output stream as a hook lifecycle observer. When set,
    /// `execute` and `execute_session_end` call
    /// [`OutputStream::emit_hook_started`] before each blocking hook and
    /// [`OutputStream::emit_hook_response`] after it completes. Non-blocking
    /// (background) hooks are excluded. Used by `--include-hook-events` in the
    /// stream-json CLI path; default `None` (behavior-neutral).
    #[must_use]
    pub fn with_hook_observer(mut self, observer: Arc<dyn OutputStream>) -> Self {
        self.hook_observer = Some(observer);
        self
    }

    async fn begin_hook_progress(
        &self,
        hook: &HookDefinition,
        hook_event: &str,
        identity: &HookAttachmentIdentity,
    ) -> Option<String> {
        let observer = self.hook_observer.as_ref()?;
        let progress_id = format!("{}:{}", hook.id, identity.tool_use_id);
        observer
            .emit_hook_progress_started(
                &progress_id,
                &hook.name,
                hook_event,
                hook.status_message.as_deref(),
            )
            .await;
        Some(progress_id)
    }

    async fn finish_hook_progress(&self, progress_id: Option<&str>) {
        if let (Some(observer), Some(progress_id)) = (&self.hook_observer, progress_id) {
            observer.emit_hook_progress_finished(progress_id).await;
        }
    }

    /// Override the SSRF guard used by HTTP hook dispatch.
    ///
    /// The default constructor keeps the production guard policy. This builder
    /// exists so integration tests can provide deterministic DNS answers
    /// without weakening the runtime defaults.
    #[must_use]
    pub fn with_ssrf_guard(mut self, ssrf_guard: SsrfGuard) -> Self {
        self.ssrf_guard = ssrf_guard;
        self
    }

    /// Attach an [`AsyncHookRegistry`] so non-blocking (`blocking == false`)
    /// hooks are backgrounded instead of awaited (B5). Without this, a
    /// non-blocking hook still runs synchronously (its result is not dropped),
    /// but the engine gains no backgrounding for it.
    #[must_use]
    pub fn with_async_registry(mut self, registry: Arc<AsyncHookRegistry>) -> Self {
        self.async_registry = Some(registry);
        self
    }

    /// Attach a [`SubagentSpawner`] so the `Agent` arm can fork subagents.
    /// Without this, `HookExecutor::Agent` hooks return a structured
    /// "not wired" error. M5-06.
    #[must_use]
    pub fn with_agent_spawner(mut self, spawner: Arc<dyn SubagentSpawner>) -> Self {
        self.agent_spawner = Some(spawner);
        self
    }

    /// Attach a [`HookPromptRunner`] so the `Prompt` arm can evaluate inline
    /// single-turn LLM queries (`execPromptHook.ts`). Without this,
    /// [`HookExecutor::Prompt`] hooks return a structured "not wired" no-op and
    /// never block. The orchestrator implements the runner over its existing
    /// one-shot `messages_create` call, keeping the hooks crate independent of
    /// the api-client.
    #[must_use]
    pub fn with_prompt_runner(mut self, runner: Arc<dyn HookPromptRunner>) -> Self {
        self.prompt_runner = Some(runner);
        self
    }

    /// Attach a [`ProcessRunner`] + [`Sandbox`] so the `Command` arm can spawn
    /// child processes. Both are required: the runner only accepts a
    /// [`platform_api::SandboxedCommand`], which only the sandbox can mint (the
    /// D2 / spec A1 sandbox-decision invariant). Without this, `Command`
    /// hooks return a structured "not wired" error.
    #[must_use]
    pub fn with_process_runner(
        mut self,
        process: Arc<dyn ProcessRunner>,
        sandbox: Arc<dyn Sandbox>,
    ) -> Self {
        self.process = Some(process);
        self.sandbox = Some(sandbox);
        self
    }

    /// Attach a name-addressed MCP invoker so the `mcp_tool` arm can call an
    /// already-connected server owned by the composition root.
    #[must_use]
    pub fn with_mcp_invoker(mut self, invoker: Arc<dyn HookMcpInvoker>) -> Self {
        self.mcp_invoker = Some(invoker);
        self
    }

    /// Register a builtin handler. Subsequent hook definitions referencing
    /// `h.id()` via [`HookExecutor::Builtin`] will dispatch to this handler.
    pub fn register_builtin(&mut self, h: Arc<dyn BuiltinHookHandler>) {
        self.builtin_handlers.insert(h.id().into(), h);
    }

    /// Borrow the shared HTTP transport.
    #[allow(dead_code)]
    pub(crate) fn http(&self) -> &Arc<dyn HttpTransport> {
        &self.http
    }

    /// Snapshot the cheaply-cloneable dispatcher backing a single hook
    /// invocation. Every field is an `Arc` / `Clone` so the snapshot can be
    /// moved into a `'static` background future (B5) or borrowed inline for the
    /// synchronous path — both go through the same [`Dispatcher::dispatch`].
    fn dispatcher(&self) -> Dispatcher {
        Dispatcher {
            http: self.http.clone(),
            ssrf_guard: self.ssrf_guard.clone(),
            http_hook_policy: self.http_hook_policy.clone(),
            builtin_handlers: self.builtin_handlers.clone(),
            agent_spawner: self.agent_spawner.clone(),
            prompt_runner: self.prompt_runner.clone(),
            process: self.process.clone(),
            sandbox: self.sandbox.clone(),
            mcp_invoker: self.mcp_invoker.clone(),
            async_registry: self.async_registry.clone(),
            attachment_sink: self.attachment_sink.clone(),
            hook_observer: self.hook_observer.clone(),
            runtime: self.runtime.clone(),
        }
    }

    /// Whether any registered hook subscribes to `event_type` (ignoring
    /// per-hook matchers). Cheap gate read over the live registry — see
    /// [`HookRegistry::has_hooks_for`]. Used by best-effort lifecycle fire
    /// paths (e.g. the CLI repl idle-prompt timer) to avoid arming work when
    /// no subscriber exists.
    pub async fn has_hooks_for(&self, event_type: &crate::events::HookEventType) -> bool {
        self.registry.read().await.has_hooks_for(event_type)
    }

    /// Register an agent's frontmatter hooks scoped to `agent_id` (G4 — claude
    /// `registerFrontmatterHooks(setAppState, agentId, hooks, …, isAgent)`,
    /// runAgent.ts:557-575). Writes through the shared registry so the hooks fire
    /// for the lifetime of the running subagent; pass `is_agent = true` to
    /// retarget `Stop` subscriptions to `SubagentStop`. Pair with
    /// [`Self::clear_agent_hooks`] in the runner's terminal/cleanup path
    /// (claude's `clearSessionHooks` in the `runAgent` finally).
    pub async fn register_agent_hooks(
        &self,
        agent_id: protocol::AgentId,
        hooks: &[HookDefinition],
        is_agent: bool,
    ) {
        self.registry
            .write()
            .await
            .register_agent_hooks(agent_id, hooks, is_agent);
    }

    /// Remove every frontmatter hook scoped to `agent_id` (G4 — claude
    /// `clearSessionHooks(rootSetAppState, agentId)`, runAgent.ts finally).
    /// Returns the number of hooks dropped.
    pub async fn clear_agent_hooks(&self, agent_id: protocol::AgentId) -> usize {
        self.registry.write().await.clear_agent_hooks(agent_id)
    }

    /// Insert or replace one named runtime hook scoped to `session_id`.
    pub async fn upsert_session_named_hook(
        &self,
        session_id: protocol::SessionId,
        name: String,
        hook: HookDefinition,
    ) -> Option<HookDefinition> {
        self.registry
            .write()
            .await
            .upsert_session_named_hook(session_id, name, hook)
    }

    /// Borrow a named runtime hook scoped to `session_id`, if present.
    pub async fn get_session_named_hook(
        &self,
        session_id: protocol::SessionId,
        name: &str,
    ) -> Option<HookDefinition> {
        self.registry
            .read()
            .await
            .get_session_named_hook(session_id, name)
            .cloned()
    }

    /// Remove one named runtime hook scoped to `session_id`.
    pub async fn remove_session_named_hook(
        &self,
        session_id: protocol::SessionId,
        name: &str,
    ) -> Option<HookDefinition> {
        self.registry
            .write()
            .await
            .remove_session_named_hook(session_id, name)
    }

    /// Publish before the child's terminal event, so parent stop hooks never
    /// race the transcript writer or accidentally evaluate the parent history.
    pub fn publish_agent_prompt_transcript(
        &self,
        session_id: protocol::SessionId,
        agent_id: protocol::AgentId,
        transcript: crate::PromptHookTranscript,
    ) {
        self.agent_prompt_transcripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((session_id, agent_id), transcript);
    }

    pub fn take_agent_prompt_transcript(
        &self,
        session_id: protocol::SessionId,
        agent_id: protocol::AgentId,
    ) -> Option<crate::PromptHookTranscript> {
        self.agent_prompt_transcripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(session_id, agent_id))
    }

    /// Remove every named runtime hook scoped to `session_id`.
    pub async fn clear_session_hooks(&self, session_id: protocol::SessionId) -> usize {
        self.agent_prompt_transcripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(session, _), _| *session != session_id);
        self.registry.write().await.clear_session_hooks(session_id)
    }

    /// Fire the `SessionEnd` hook batch against a *batch-wide shutdown deadline*
    /// (claude-code `lje` → `cH({…, signal: AbortSignal.timeout(Wqt())})`,
    /// BIN off 205706285 / 205715763).
    ///
    /// Distinct from [`Self::execute`]: at session teardown claude-code does not
    /// give the `SessionEnd` batch the generic 10-minute per-hook budget — it caps
    /// the WHOLE batch with a single deadline so a slow / hung `SessionEnd` hook
    /// cannot stall process exit. The deadline is
    /// [`session_end_batch_timeout_ms`] (env override, else
    /// `max(1500, min(max_per_hook_ms, 60000))`).
    ///
    /// The deadline is applied as a wall-clock budget shared across the batch:
    /// each matched hook is dispatched under `min(remaining_deadline, its own
    /// per-hook timeout)`, mirroring the binary, where every hook's effective
    /// signal is `xP(batchSignal, {timeoutMs: perHook})` — abort on EITHER the
    /// batch deadline OR the per-hook timeout (BIN off 205755512 / 200906242).
    /// Once the batch deadline elapses, the remaining hooks see an already-fired
    /// deadline and are skipped (the binary's `if(r?.aborted)return[]` / empty
    /// per-hook output), so a hung early hook cannot starve teardown.
    ///
    /// Like [`Self::execute`]: priority-descending order, every matched hook
    /// dispatched (no first-`Block` break — #45(b); `Block` is folded sticky),
    /// `once` removal, B5 non-blocking backgrounding (those are not awaited so
    /// they never consume the deadline), and a strict no-op (default aggregate)
    /// when no `SessionEnd` hook is registered. The batch still stops early when
    /// the shared shutdown deadline elapses.
    pub async fn execute_session_end(
        &self,
        event: HookEvent,
        ctx: HookContext,
    ) -> AggregateHookResult {
        // #41 runner-head gate (`h$`): `policySettings.disableAllHooks` skips the
        // SessionEnd batch entirely (the binary's `cH` head runs before the
        // SessionEnd deadline race too).
        if let Some(skipped) = self.policy_disable_gate(&event) {
            return skipped;
        }
        let reg = self.registry.read().await;
        let matched: Vec<HookDefinition> =
            reg.match_event(&event, &ctx).into_iter().cloned().collect();
        drop(reg);

        // Compute the batch deadline from the env override / the max declared
        // per-hook timeout across the matched set (claude-code `Wqt`).
        let env_value = match &self.session_end_timeout_override {
            Some(override_ms) => Some(override_ms.clone()),
            None => std::env::var(SESSION_END_HOOKS_TIMEOUT_ENV).ok(),
        };
        let batch_timeout_ms =
            session_end_batch_timeout_ms(env_value.as_deref(), max_per_hook_timeout_ms(&matched));
        let deadline = tokio::time::Instant::now() + Duration::from_millis(batch_timeout_ms);

        let mut agg = AggregateHookResult::default();
        let hook_event = format!("{:?}", event.event_type());
        // One `toolUseID` per dispatch, shared by every matched hook.
        let attachment_id = attachment_identity(&event);
        for hook in &matched {
            agg.progress.push(crate::events::HookProgressEvent {
                hook_event: hook_event.clone(),
                hook_name: hook.name.clone(),
                status_message: hook.status_message.clone(),
            });
            if hook.blocking {
                let progress_id = self
                    .begin_hook_progress(hook, &hook_event, &attachment_id)
                    .await;
                // Emit hook_started BEFORE dispatch (for --include-hook-events).
                if let Some(observer) = &self.hook_observer {
                    observer
                        .emit_hook_started(&hook.id.to_string(), &hook.name, &hook_event)
                        .await;
                }
                // Bound this hook by the remaining batch budget. Once the batch
                // deadline has passed, `timeout_at` fires immediately, so the
                // remaining hooks are skipped (the binary's already-aborted
                // signal → empty output), and the batch stops — a hung early
                // hook cannot starve teardown.
                let run_started = std::time::Instant::now();
                let Ok(result) = tokio::time::timeout_at(
                    deadline,
                    self.dispatcher()
                        .dispatch(hook, &event, &ctx, progress_id.as_deref()),
                )
                .await
                else {
                    // Batch deadline elapsed mid-dispatch (or before this hook
                    // started). Record a timeout outcome and stop the batch —
                    // no later SessionEnd hook gets a turn, faithful to the
                    // aborted batch signal.
                    emit_session_end_batch_timeout(hook, batch_timeout_ms);
                    let timed_out = HookResult {
                        outcome: HookOutcome::Timeout,
                        stdout: String::new(),
                        stderr: format!(
                            "SessionEnd hook {} aborted: batch deadline ({batch_timeout_ms}ms) exceeded",
                            hook.id
                        ),
                        exit_code: None,
                        response: None,
                    };
                    // Emit timeout hook_response before merging.
                    if let Some(observer) = &self.hook_observer {
                        observer
                            .emit_hook_response(
                                &hook.id.to_string(),
                                &hook.name,
                                &hook_event,
                                "",
                                &timed_out.stdout,
                                &timed_out.stderr,
                                timed_out.exit_code,
                                "timeout",
                            )
                            .await;
                    }
                    self.finish_hook_progress(progress_id.as_deref()).await;
                    // hook runs are bounded by the batch deadline — fits u64
                    #[allow(clippy::cast_possible_truncation)]
                    let run_ms = run_started.elapsed().as_millis() as u64;
                    self.publish_run_attachment(&mut agg, hook, &attachment_id, &timed_out, run_ms)
                        .await;
                    Self::merge(&mut agg, hook, timed_out, &hook_event, self.eval_confined);
                    break;
                };
                // Emit hook_response AFTER dispatch (for --include-hook-events).
                if let Some(observer) = &self.hook_observer {
                    let outcome_str = match result.outcome {
                        HookOutcome::Success => "success",
                        HookOutcome::Error => "error",
                        HookOutcome::Cancelled => "error",
                        HookOutcome::Timeout => "timeout",
                    };
                    let resp_text = result
                        .response
                        .as_ref()
                        .and_then(|r| r.system_message.as_deref())
                        .unwrap_or("");
                    let combined_output = if resp_text.is_empty() {
                        result.stdout.clone()
                    } else if result.stdout.is_empty() {
                        resp_text.to_string()
                    } else {
                        format!("{}\n{}", result.stdout, resp_text)
                    };
                    observer
                        .emit_hook_response(
                            &hook.id.to_string(),
                            &hook.name,
                            &hook_event,
                            &combined_output,
                            &result.stdout,
                            &result.stderr,
                            result.exit_code,
                            outcome_str,
                        )
                        .await;
                }
                if !is_runtime_async_backgrounded(&result) {
                    self.finish_hook_progress(progress_id.as_deref()).await;
                }
                // hook runs are bounded by the batch deadline — fits u64
                #[allow(clippy::cast_possible_truncation)]
                let run_ms = run_started.elapsed().as_millis() as u64;
                if hook.once && matches!(result.outcome, HookOutcome::Success) {
                    self.registry.write().await.remove_once_hook(hook.id);
                }
                // ONE transcript attachment per hook run — same as `execute`.
                self.publish_run_attachment(&mut agg, hook, &attachment_id, &result, run_ms)
                    .await;
                Self::merge(&mut agg, hook, result, &hook_event, self.eval_confined);
                // #45(b): no early break on first `Block`. SessionEnd's decision is
                // a shutdown-path verdict that is never consumed for blocking, and
                // claude's `cH` runner runs every matched hook regardless; running
                // the rest of the batch (still bounded by the batch deadline above)
                // preserves later SessionEnd hooks' side effects. The `break` above
                // remains for the batch-deadline-elapsed case only.
            } else {
                // B5 non-blocking hooks are backgrounded (not awaited), so they
                // never consume the batch deadline — identical to `execute`.
                if let Some(attachment) = self.background_hook(hook, &event, &ctx).await {
                    agg.hook_attachments.push(attachment);
                }
            }
        }
        agg
    }

    /// Fire `event` and return the aggregated result of every matching hook.
    ///
    /// Hooks are evaluated in priority-descending order; EVERY matched hook is
    /// dispatched (no early break on the first `Block` — #45(b), mirroring
    /// claude-code's `cH` runner). The aggregate verdict is `Block` if ANY hook
    /// blocked (`merge` keeps `Block` sticky), so later hooks' side effects
    /// (systemMessage / additionalContext / updatedInput / `once`-removal) are
    /// preserved while the blocking decision is unchanged.
    ///
    /// B5 — a matched hook with `blocking == false` is routed to the
    /// [`AsyncHookRegistry`] (when wired) instead of being awaited: it is
    /// backgrounded, EXCLUDED from the aggregate, and so can NEVER contribute a
    /// `Block` decision. A `blocking == true` hook still runs synchronously,
    /// exactly as before — the regression-guarded common case. (When no async
    /// registry is wired, a non-blocking hook degrades to running synchronously
    /// so its result is not silently dropped, but it is STILL excluded from the
    /// aggregate to preserve the "non-blocking can't block" contract.)
    pub async fn execute(&self, event: HookEvent, ctx: HookContext) -> AggregateHookResult {
        self.execute_excluding_hook(event, ctx, None).await
    }

    /// Skip a deferred session goal before dispatch, preserving all other hooks.
    pub async fn execute_excluding_hook(
        &self,
        event: HookEvent,
        ctx: HookContext,
        excluded: Option<protocol::HookId>,
    ) -> AggregateHookResult {
        // #41 runner-head gate (`h$`): `policySettings.disableAllHooks` skips
        // ALL hooks before any matching/dispatch.
        if let Some(skipped) = self.policy_disable_gate(&event) {
            return skipped;
        }
        let reg = self.registry.read().await;
        let matched: Vec<HookDefinition> = reg
            .match_event(&event, &ctx)
            .into_iter()
            .filter(|hook| Some(hook.id) != excluded)
            .cloned()
            .collect();
        drop(reg);
        let mut agg = AggregateHookResult::default();
        let hook_event = format!("{:?}", event.event_type());
        // One `toolUseID` per dispatch, shared by every matched hook.
        let attachment_id = attachment_identity(&event);
        for hook in &matched {
            // Emit a `hook_progress` event for every matching hook *before* it
            // runs, carrying the per-hook `status_message` (claude-code
            // `utils/hooks.ts:2094-2116`). The spinner substitutes this text
            // for the generic running line when present.
            agg.progress.push(crate::events::HookProgressEvent {
                hook_event: hook_event.clone(),
                hook_name: hook.name.clone(),
                status_message: hook.status_message.clone(),
            });
            if hook.blocking {
                let progress_id = self
                    .begin_hook_progress(hook, &hook_event, &attachment_id)
                    .await;
                // Emit hook_started BEFORE dispatch (for --include-hook-events).
                if let Some(observer) = &self.hook_observer {
                    observer
                        .emit_hook_started(&hook.id.to_string(), &hook.name, &hook_event)
                        .await;
                }
                // Synchronous path — unchanged from M5-06.
                let run_started = std::time::Instant::now();
                let result = self
                    .dispatcher()
                    .dispatch(hook, &event, &ctx, progress_id.as_deref())
                    .await;
                // hook runs are bounded by the runner timeout — u128 ms fits u64
                #[allow(clippy::cast_possible_truncation)]
                let run_ms = run_started.elapsed().as_millis() as u64;
                // Emit hook_response AFTER dispatch (for --include-hook-events).
                if let Some(observer) = &self.hook_observer {
                    let outcome_str = match result.outcome {
                        HookOutcome::Success => "success",
                        HookOutcome::Error => "error",
                        HookOutcome::Cancelled => "error",
                        HookOutcome::Timeout => "timeout",
                    };
                    // `output` = combined response text (stdout + any systemMessage).
                    let resp_text = result
                        .response
                        .as_ref()
                        .and_then(|r| r.system_message.as_deref())
                        .unwrap_or("");
                    let combined_output = if resp_text.is_empty() {
                        result.stdout.clone()
                    } else if result.stdout.is_empty() {
                        resp_text.to_string()
                    } else {
                        format!("{}\n{}", result.stdout, resp_text)
                    };
                    observer
                        .emit_hook_response(
                            &hook.id.to_string(),
                            &hook.name,
                            &hook_event,
                            &combined_output,
                            &result.stdout,
                            &result.stderr,
                            result.exit_code,
                            outcome_str,
                        )
                        .await;
                }
                if !is_runtime_async_backgrounded(&result) {
                    self.finish_hook_progress(progress_id.as_deref()).await;
                }
                // `once` runtime removal (claude-code `registerSkillHooks.ts:35-36`,
                // `utils/hooks.ts:2918-2919`): drop the hook from the registry
                // only after it runs with a *success* outcome, so it never fires
                // again. An erroring `once` hook is left in place.
                if hook.once && matches!(result.outcome, HookOutcome::Success) {
                    self.registry.write().await.remove_once_hook(hook.id);
                }
                // ONE transcript attachment per hook run (claude persists one
                // for every run — 26 048 records in real 2.1.220 transcripts).
                self.publish_run_attachment(&mut agg, hook, &attachment_id, &result, run_ms)
                    .await;
                Self::merge(&mut agg, hook, result, &hook_event, self.eval_confined);
                // #45(b): NO early break on the first `Block`. claude-code's `cH`
                // runner dispatches every matched hook (BIN off 205755512) and
                // folds `blocked = some(t.blocked)` afterwards, so later hooks'
                // systemMessage / additionalContext / updatedInput / `once`-removal
                // side effects must NOT be lost. `merge` makes `Block` sticky, so
                // the aggregate verdict is identical to the old short-circuit (any
                // block wins) while the side effects are now preserved.
            } else {
                // B5 config-`async` path: background the hook and continue. It
                // is excluded from `agg`, so it cannot block.
                if let Some(attachment) = self.background_hook(hook, &event, &ctx).await {
                    agg.hook_attachments.push(attachment);
                }
            }
        }
        publish_classifier_host_contexts(&agg, &attachment_id);
        agg
    }

    /// Fire `event` against ONLY the frontmatter hooks scoped to `agent_id`
    /// (source / plugin / other-agent buckets excluded).
    ///
    /// The child runner uses this to fire `SubagentStop` for its OWN frontmatter
    /// Stop→SubagentStop hooks (claude fires the subagent's stop hooks inside the
    /// child, runAgent.ts) at the loop's end, BEFORE [`Self::clear_agent_hooks`]
    /// removes them — otherwise those retargeted hooks are dead code (registered,
    /// then cleared before the orchestrator-side `SubagentStop` chokepoint runs).
    /// Scoping to this agent's bucket avoids double-firing session / plugin
    /// `SubagentStop` hooks, which the chokepoint already covers. A strict no-op
    /// (no matched hooks) when the agent registered none, so the no-frontmatter
    /// path is byte-identical to legacy.
    ///
    /// Mirrors [`Self::execute`]'s per-hook dispatch (progress beacon, `blocking`
    /// vs. background routing, `once` removal, no-early-`Block`-break with a
    /// sticky `Block` fold — #45(b)) so a frontmatter hook behaves identically
    /// whether it fires here or via the general path.
    pub async fn execute_agent_scoped(
        &self,
        event: HookEvent,
        ctx: HookContext,
        agent_id: protocol::AgentId,
    ) -> AggregateHookResult {
        // #41 runner-head gate (`h$`).
        if let Some(skipped) = self.policy_disable_gate(&event) {
            return skipped;
        }
        let reg = self.registry.read().await;
        let matched: Vec<HookDefinition> = reg
            .match_event_agent_scoped(&event, agent_id)
            .into_iter()
            .cloned()
            .collect();
        drop(reg);
        let mut agg = AggregateHookResult::default();
        let hook_event = format!("{:?}", event.event_type());
        // One `toolUseID` per dispatch, shared by every matched hook.
        let attachment_id = attachment_identity(&event);
        for hook in &matched {
            agg.progress.push(crate::events::HookProgressEvent {
                hook_event: hook_event.clone(),
                hook_name: hook.name.clone(),
                status_message: hook.status_message.clone(),
            });
            if hook.blocking {
                let progress_id = self
                    .begin_hook_progress(hook, &hook_event, &attachment_id)
                    .await;
                let run_started = std::time::Instant::now();
                let result = self
                    .dispatcher()
                    .dispatch(hook, &event, &ctx, progress_id.as_deref())
                    .await;
                if !is_runtime_async_backgrounded(&result) {
                    self.finish_hook_progress(progress_id.as_deref()).await;
                }
                // hook runs are bounded by the runner timeout — u128 ms fits u64
                #[allow(clippy::cast_possible_truncation)]
                let run_ms = run_started.elapsed().as_millis() as u64;
                if hook.once && matches!(result.outcome, HookOutcome::Success) {
                    self.registry.write().await.remove_once_hook(hook.id);
                }
                // ONE transcript attachment per hook run — same as `execute`.
                self.publish_run_attachment(&mut agg, hook, &attachment_id, &result, run_ms)
                    .await;
                Self::merge(&mut agg, hook, result, &hook_event, self.eval_confined);
                // #45(b): no early break on first `Block` — see `execute`. All
                // matched hooks dispatch; `merge` keeps `Block` sticky.
            } else if let Some(attachment) = self.background_hook(hook, &event, &ctx).await {
                agg.hook_attachments.push(attachment);
            }
        }
        agg
    }

    /// Fire `event` against every matching hook EXCEPT the frontmatter bucket
    /// scoped to `exclude_agent_id` (source / plugin / other-agent buckets still
    /// fire).
    ///
    /// The orchestrator-side `SubagentStop` chokepoint uses this so it fires the
    /// session / plugin `SubagentStop` hooks WITHOUT re-firing the child's OWN
    /// frontmatter `Stop`→`SubagentStop` hooks — those already fired in-child via
    /// [`Self::execute_agent_scoped`] (claude fires a subagent's stop hooks inside
    /// `runAgent`). Excluding by id is race-free vs. the runner's
    /// `clear_agent_hooks` (which removes the bucket only after the terminal
    /// event). A strict no-op difference from [`Self::execute`] when the excluded
    /// agent registered no frontmatter hooks (every `FakeAgentTool` fixture).
    ///
    /// Mirrors [`Self::execute`]'s per-hook dispatch (progress beacon, `blocking`
    /// vs. background routing, `once` removal, no-early-`Block`-break with a
    /// sticky `Block` fold — #45(b)).
    pub async fn execute_excluding_agent(
        &self,
        event: HookEvent,
        mut ctx: HookContext,
        exclude_agent_id: protocol::AgentId,
    ) -> AggregateHookResult {
        if matches!(&event, HookEvent::SubagentStop { agent_id, .. } if *agent_id == exclude_agent_id)
        {
            // Consume even if hooks are disabled or none match.
            ctx.prompt_transcript =
                self.take_agent_prompt_transcript(ctx.session_id, exclude_agent_id);
        }
        // #41 runner-head gate (`h$`).
        if let Some(skipped) = self.policy_disable_gate(&event) {
            return skipped;
        }
        let reg = self.registry.read().await;
        let matched: Vec<HookDefinition> = reg
            .match_event_excluding_agent(&event, exclude_agent_id)
            .into_iter()
            .cloned()
            .collect();
        drop(reg);
        let mut agg = AggregateHookResult::default();
        let hook_event = format!("{:?}", event.event_type());
        // One `toolUseID` per dispatch, shared by every matched hook.
        let attachment_id = attachment_identity(&event);
        for hook in &matched {
            agg.progress.push(crate::events::HookProgressEvent {
                hook_event: hook_event.clone(),
                hook_name: hook.name.clone(),
                status_message: hook.status_message.clone(),
            });
            if hook.blocking {
                let progress_id = self
                    .begin_hook_progress(hook, &hook_event, &attachment_id)
                    .await;
                let run_started = std::time::Instant::now();
                let result = self
                    .dispatcher()
                    .dispatch(hook, &event, &ctx, progress_id.as_deref())
                    .await;
                if !is_runtime_async_backgrounded(&result) {
                    self.finish_hook_progress(progress_id.as_deref()).await;
                }
                // hook runs are bounded by the runner timeout — u128 ms fits u64
                #[allow(clippy::cast_possible_truncation)]
                let run_ms = run_started.elapsed().as_millis() as u64;
                if hook.once && matches!(result.outcome, HookOutcome::Success) {
                    self.registry.write().await.remove_once_hook(hook.id);
                }
                // ONE transcript attachment per hook run — same as `execute`.
                self.publish_run_attachment(&mut agg, hook, &attachment_id, &result, run_ms)
                    .await;
                Self::merge(&mut agg, hook, result, &hook_event, self.eval_confined);
                // #45(b): no early break on first `Block` — see `execute`. All
                // matched hooks dispatch; `merge` keeps `Block` sticky.
            } else if let Some(attachment) = self.background_hook(hook, &event, &ctx).await {
                agg.hook_attachments.push(attachment);
            }
        }
        agg
    }

    /// Build + publish the transcript attachments for a completed hook run.
    ///
    /// Pushed onto `agg.hook_attachments` and, when a sink is wired, persisted
    /// through it.
    ///
    /// Up to TWO records are emitted, in the oracle's order (BIN off
    /// **237807875**, the runner `uL`'s per-result loop):
    ///
    /// 1. the run-OUTCOME record (`hook_success` / `hook_non_blocking_error` /
    ///    `hook_cancelled`), from `if(q.message)yield{message:q.message,…}`. A
    ///    BLOCKING run contributes none — the oracle yields a bare
    ///    `{blockingError, outcome:"blocking"}` signal there (BIN off
    ///    **237805098**) and it is the CALLER that builds the
    ///    `hook_blocking_error` record (BIN off 234726074).
    /// 2. O2 — a `hook_system_message` record, from the SEPARATE
    ///    `if(q.systemMessage){…}` immediately after. Because those two `if`s
    ///    are independent in the oracle, a hook that BLOCKS *and* sets
    ///    `systemMessage` still emits this second record; hence the run-outcome
    ///    `None` must not short-circuit it.
    async fn publish_run_attachment(
        &self,
        agg: &mut AggregateHookResult,
        hook: &HookDefinition,
        id: &HookAttachmentIdentity,
        result: &HookResult,
        elapsed_ms: u64,
    ) {
        if let Some(value) = build_run_attachment_persisting(
            self.attachment_sink.as_ref(),
            hook,
            id,
            result,
            elapsed_ms,
        )
        .await
        {
            self.publish_attachment(agg, value).await;
        }
        // O2: `hook_system_message` — transcript + TUI only. Its renderer entry
        // is `hook_system_message:()=>[]` (BIN off 238109329), so this record
        // must NEVER reach the model; it exists so the systemMessage the hook
        // emitted is recoverable from the transcript. The oracle guards on
        // truthiness (`if(q.systemMessage)`), so an empty string emits nothing.
        // ONE record per hook result, not one per aggregate.
        let system_message = result
            .response
            .as_ref()
            .and_then(|r| r.system_message.as_deref())
            .unwrap_or_default();
        if !system_message.is_empty() {
            let mut value = attachment::system_message_attachment(id, system_message);
            if let Some(reference) =
                persist_large_hook_output(self.attachment_sink.as_ref(), system_message).await
            {
                value["content"] = serde_json::Value::String(reference);
            }
            self.publish_attachment(agg, value).await;
        }
    }

    /// Carry one attachment on the aggregate and, when wired, the sink.
    async fn publish_attachment(&self, agg: &mut AggregateHookResult, value: serde_json::Value) {
        agg.hook_attachments.push(value.clone());
        if let Some(sink) = &self.attachment_sink {
            sink.record(value).await;
        }
    }

    /// Route a `blocking == false` hook to the background async registry (B5).
    ///
    /// Mirrors claude-code `executeInBackground` (`utils/hooks.ts:995-1030`):
    /// the engine proceeds immediately and the hook's eventual result folds
    /// back through the registry's completion channel. When no registry is
    /// wired the hook degrades to a synchronous run whose result is returned
    /// only as an attachment (it still cannot block) — this keeps a
    /// misconfigured engine from silently no-op'ing the hook entirely.
    async fn background_hook(
        &self,
        hook: &HookDefinition,
        event: &HookEvent,
        ctx: &HookContext,
    ) -> Option<serde_json::Value> {
        let identity = attachment_identity(event);
        let hook_event = format!("{:?}", event.event_type());
        let progress_id = self.begin_hook_progress(hook, &hook_event, &identity).await;
        let Some(registry) = &self.async_registry else {
            // No registry wired: run inline but exclude the decision from the
            // aggregate so the "non-blocking can't block" contract still holds.
            let run_started = std::time::Instant::now();
            let result = self
                .dispatcher()
                .dispatch(hook, event, ctx, progress_id.as_deref())
                .await;
            if hook.once && matches!(result.outcome, HookOutcome::Success) {
                self.registry.write().await.remove_once_hook(hook.id);
            }
            #[allow(clippy::cast_possible_truncation)]
            let run_ms = run_started.elapsed().as_millis() as u64;
            let attachment = build_run_attachment_persisting(
                self.attachment_sink.as_ref(),
                hook,
                &identity,
                &result,
                run_ms,
            )
            .await;
            if let (Some(sink), Some(value)) = (&self.attachment_sink, &attachment) {
                sink.record(value.clone()).await;
            }
            self.finish_hook_progress(progress_id.as_deref()).await;
            return attachment;
        };
        let dispatcher = self.dispatcher();
        let hook_owned = hook.clone();
        let event_owned = event.clone();
        let ctx_owned = ctx.clone();
        let hook_registry = self.registry.clone();
        let hook_id = hook.id;
        let once = hook.once;
        let dispatch_progress_id = progress_id.clone();
        let rewake_message = hook
            .async_rewake
            .then(|| hook.rewake_message.clone().unwrap_or_default());
        let completion = attachment_completion(
            self.attachment_sink.clone(),
            hook_owned.clone(),
            identity,
            std::time::Instant::now(),
            self.hook_observer.clone(),
            progress_id.clone(),
        );
        let work: HookWork = Box::pin(async move {
            let result = dispatcher
                .dispatch(
                    &hook_owned,
                    &event_owned,
                    &ctx_owned,
                    dispatch_progress_id.as_deref(),
                )
                .await;
            if once && matches!(result.outcome, HookOutcome::Success) {
                hook_registry.write().await.remove_once_hook(hook_id);
            }
            result
        });
        if let Err(e) = registry
            .spawn_with_completion_and_rewake(
                hook.id,
                hook.async_timeout,
                work,
                completion,
                rewake_message,
            )
            .await
        {
            self.finish_hook_progress(progress_id.as_deref()).await;
            tracing::warn!(
                hook_id = %hook.id,
                error = %e,
                "failed to background async hook; it will not run",
            );
        }
        None
    }
}

/// Cheaply-cloneable snapshot of the executor dependencies needed to run a
/// single hook. Built by [`HookExecutorImpl::dispatcher`]. Because every field
/// is an `Arc` / `Clone`, a `Dispatcher` can be moved into a `'static`
/// background future (B5 async path) or borrowed inline for the synchronous
/// path — both reach the identical [`Self::dispatch`] arm logic.
#[derive(Clone)]
struct Dispatcher {
    http: Arc<dyn HttpTransport>,
    ssrf_guard: SsrfGuard,
    /// H-BIN-12 HTTP-hook security policy, snapshotted alongside the transport
    /// so the async (B5) and synchronous dispatch paths share one policy.
    http_hook_policy: HttpHookPolicy,
    builtin_handlers: HashMap<String, Arc<dyn BuiltinHookHandler>>,
    agent_spawner: Option<Arc<dyn SubagentSpawner>>,
    prompt_runner: Option<Arc<dyn HookPromptRunner>>,
    process: Option<Arc<dyn ProcessRunner>>,
    sandbox: Option<Arc<dyn Sandbox>>,
    mcp_invoker: Option<Arc<dyn HookMcpInvoker>>,
    /// P2-09: background registry for the runtime `{"async":true}` marker
    /// fold-back. When a `Command` hook prints the marker its first stdout line,
    /// the runner backgrounds it and hands back an eventual-output handle; the
    /// Command arm registers that handle here so the hook's eventual output
    /// re-injects as an `async_hook_response` on a later turn (claude-code
    /// `registerPendingAsyncHook`). `None` ⇒ the marker path degrades to a
    /// no-op synchronous decision (the hook still ran, but nothing folds back).
    async_registry: Option<Arc<AsyncHookRegistry>>,
    /// Transcript sink needed by runtime-marker completions. The synchronous
    /// caller has already returned by the time their real output is available.
    attachment_sink: Option<Arc<dyn HookAttachmentSink>>,
    /// Live progress sink retained for runtime-marker hooks whose real command
    /// completion happens after the originating dispatch has returned.
    hook_observer: Option<Arc<dyn OutputStream>>,
    /// SH-07: background spawner for the `hook_progress` poll task. The hooks
    /// crate must not touch tokio directly (D17), so the 1 s cadence runs on the
    /// injected [`RuntimeSpawner`] exactly like the async-hook registry's work.
    runtime: Arc<dyn RuntimeSpawner>,
}

impl Dispatcher {
    /// SH-07 — start claude-code's `hook_progress` poll for one command hook.
    ///
    /// Oracle 2.1.238 @ 296463298:
    /// ```js
    /// function tWi(e){ if(!Q9i(e.hookEvent))return()=>{};
    ///   let t="",r=setInterval(()=>{ e.getOutput().then(({stdout:n,stderr:o,output:i})=>{
    ///     if(i===t)return; t=i, EjT({…,stdout:n,stderr:o,output:i}) }) },
    ///   e.intervalMs??1000); return r.unref(),()=>clearInterval(r) }
    /// ```
    /// i.e. a 1 s interval that emits a frame ONLY when the accumulated
    /// `output` changed — so a hook that finishes inside the first second emits
    /// nothing, and a silent long-running hook emits nothing either.
    ///
    /// Upstream's `if(!Q9i(e.hookEvent))return()=>{}` head skips the poll
    /// entirely when hook events are not being streamed; the port asks the sink
    /// the same question through [`OutputStream::hook_events_streamed`], so a
    /// host that is not streaming hook frames pays for neither the poll task nor
    /// the chunked pipe reads.
    ///
    /// Returns `None` when no poll was started; the caller then passes `None` as
    /// the runner's observer and the child is drained with the cheap bulk read.
    async fn begin_hook_progress_frames(
        &self,
        hook: &HookDefinition,
        hook_event: &str,
    ) -> Option<HookProgressFrames> {
        let observer = self.hook_observer.as_ref()?.clone();
        // `if(!Q9i(e.hookEvent))return()=>{}` — no poll, no live pipe reads, when
        // the sink is not streaming hook frames for this event. Every
        // non-stream-json host answers `false` by default, so the desktop/TUI
        // path pays nothing.
        if !observer.hook_events_streamed(hook_event) {
            return None;
        }
        let buf = Arc::new(std::sync::Mutex::new(HookLiveOutput::default()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let poll_buf = buf.clone();
        let poll_stop = stop.clone();
        let hook_id = hook.id.to_string();
        let hook_name = hook.name.clone();
        let event = hook_event.to_string();
        let sleeper = self.runtime.clone();
        let task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> =
            Box::pin(async move {
                // `let t=""` — the last emitted aggregate.
                let mut last: Option<String> = None;
                loop {
                    sleeper
                        .sleep(Duration::from_millis(HOOK_PROGRESS_INTERVAL_MS))
                        .await;
                    if poll_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    let (stdout, stderr, output) = {
                        let live = poll_buf.lock().unwrap_or_else(|e| e.into_inner());
                        live.snapshot()
                    };
                    // `if(i===t)return` — no frame unless the aggregate moved.
                    if last.as_deref() == Some(output.as_str()) {
                        continue;
                    }
                    last = Some(output.clone());
                    observer
                        .emit_hook_progress_frame(
                            &hook_id, &hook_name, &event, &stdout, &stderr, &output,
                        )
                        .await;
                }
            });
        let handle = self.runtime.spawn("hook_progress", task).await.ok();
        Some(HookProgressFrames { buf, stop, handle })
    }

    /// SH-07 — the `()=>clearInterval(r)` disposer returned by `tWi`.
    async fn finish_hook_progress_frames(&self, frames: Option<HookProgressFrames>) {
        let Some(frames) = frames else { return };
        frames
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = &frames.handle {
            let _ = self.runtime.cancel(handle).await;
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "arm dispatch fan-out — splitting hurts readability"
    )]
    async fn dispatch(
        &self,
        hook: &HookDefinition,
        event: &HookEvent,
        ctx: &HookContext,
        progress_id: Option<&str>,
    ) -> HookResult {
        match &hook.executor {
            HookExecutor::Function { source, budget_ms } => {
                let Some((expected_event, body)) = build_envelope_body(event, ctx) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Hook {} failed: no payload shape for this event", hook.id),
                        exit_code: None,
                        response: None,
                    };
                };
                let Ok(payload) = serde_json::from_str::<serde_json::Value>(&body) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Hook {} failed: payload is not JSON", hook.id),
                        exit_code: None,
                        response: None,
                    };
                };
                let sandbox = crate::function_hook::Sandbox {
                    budget: budget_ms.map_or(
                        crate::function_hook::DEFAULT_FUNCTION_HOOK_BUDGET,
                        std::time::Duration::from_millis,
                    ),
                    ..crate::function_hook::Sandbox::default()
                };
                // Evaluated on a blocking thread: QuickJS is synchronous and a
                // hook may burn its whole budget, which must not stall the
                // async runtime the turn is running on.
                let source = source.clone();
                // `eval_abandonable`, NOT a plain `eval`: a runaway regexp
                // cannot be interrupted in-engine, so the deadline has to live
                // outside the engine, and the thread it gives up on must not be
                // one the blocking pool needs.
                let evaluated = tokio::task::spawn_blocking(move || {
                    sandbox.eval_abandonable(&source, &payload)
                })
                .await
                .unwrap_or_else(|join| {
                    Err(crate::function_hook::FunctionHookError::EngineUnavailable(
                        join.to_string(),
                    ))
                });
                match evaluated {
                    Ok(value) => {
                        let stdout = serde_json::to_string(&value).unwrap_or_default();
                        // Reuse the SAME response parser every other arm uses:
                        // a function hook must not get a private decision
                        // dialect the rest of the system does not understand.
                        let response = parse_response(&stdout, expected_event).ok();
                        HookResult {
                            outcome: HookOutcome::Success,
                            stdout,
                            stderr: String::new(),
                            exit_code: Some(0),
                            response,
                        }
                    }
                    // ⛔ A hook that throws, times out or runs out of memory is
                    // an ERROR, never a silent allow: a plugin must not be able
                    // to wave a spawn through by crashing.
                    Err(error) => HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        // ⛔ Truncate: the message is plugin-authored and
                        // unbounded (`throw new Error('A'.repeat(4e6))` measured
                        // at 4 MB), and `stderr` reaches the transcript verbatim
                        // — `persist_large_hook_output` covers only stdout.
                        stderr: crate::response::truncate_utf16(
                            &format!("Hook {} failed: {error}", hook.id),
                            FUNCTION_HOOK_STDERR_CAP,
                        ),
                        exit_code: None,
                        response: None,
                    },
                }
            }
            HookExecutor::Builtin { handler_id } => {
                if let Some(h) = self.builtin_handlers.get(handler_id) {
                    return h.handle(event, ctx).await;
                }
                HookResult {
                    outcome: HookOutcome::Error,
                    stdout: String::new(),
                    stderr: format!("builtin {handler_id} not found"),
                    exit_code: None,
                    response: None,
                }
            }
            HookExecutor::Http {
                url,
                headers,
                timeout,
                ..
            } => {
                // Resolve event-specific expected_event marker + serialize payload.
                let Some((expected_event, body)) = build_envelope_body(event, ctx) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: HTTP arm only supports PreToolUse / PostToolUse",
                            hook.id
                        ),
                        exit_code: None,
                        response: None,
                    };
                };
                let effective = if timeout.is_zero() {
                    Duration::from_millis(HOOK_HTTP_TIMEOUT_MS)
                } else {
                    *timeout
                };
                let exec = HttpExecutor {
                    http: self.http.clone(),
                    ssrf_guard: self.ssrf_guard.clone(),
                    timeout: effective,
                    policy: self.http_hook_policy.clone(),
                };
                let outcome = exec
                    .execute(hook, url, headers, &body, expected_event)
                    .await;
                emit_http_signal(hook, &outcome.signal, effective);
                outcome.result
            }
            HookExecutor::Command {
                command,
                args,
                env,
                cwd,
                shell,
            } => {
                // Both the runner and the sandbox must be wired: the runner
                // only accepts a `SandboxedCommand`, which only the sandbox can
                // mint (D2 / spec A1). Either missing ⇒ structured "not wired".
                let (Some(process), Some(sandbox)) = (&self.process, &self.sandbox) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Hook {} failed: command executor not wired", hook.id),
                        exit_code: None,
                        response: None,
                    };
                };
                let Some((expected_event, body)) = build_envelope_body(event, ctx) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: Command arm only supports PreToolUse / PostToolUse",
                            hook.id
                        ),
                        exit_code: None,
                        response: None,
                    };
                };
                // Mirror the HTTP arm's zero-duration handling: an explicit
                // `timeout: 0` (or no timeout) falls back to the 10-minute
                // default rather than instantly timing out the child.
                let effective_timeout = match hook.timeout {
                    Some(t) if !t.is_zero() => t,
                    _ => Duration::from_millis(HOOK_COMMAND_TIMEOUT_MS),
                };
                // B2: inject `LINGXI_PROJECT_DIR` into the child env so hook
                // scripts referencing `$LINGXI_PROJECT_DIR` resolve to the
                // stable project root. claude-code builds the env as
                // `{ ...subprocessEnv(), LINGXI_PROJECT_DIR: toHookPath(projectDir) }`
                // (`utils/hooks.ts:882-885`): the engine value is spread AFTER
                // the base env, so it wins over any pre-existing entry. We
                // mirror that precedence — start from the hook's declared `env`
                // (our analog of the base/subprocess env), then `insert` the
                // engine value last so it overwrites a user-supplied
                // `LINGXI_PROJECT_DIR`. The value is the stable project root,
                // falling back to `ctx.cwd` when no root is wired yet
                // (`HookContext.project_dir == None`).
                //
                // Divergence (documented, not a gap): Windows `toHookPath`
                // POSIX-path conversion is skipped — macOS/Linux parity target,
                // consistent with `turn_loop.rs::absolutize`. Full
                // `subprocessEnv()` base-env replication and `CLAUDE_ENV_FILE`
                // are out of B2 scope.
                let mut child_env = env.clone();
                // #43: `...Uot(o)` child-session env spread. claude-code assembles
                // the hook command env as `P={...WO(), ...Uot(o), LINGXI_PROJECT_DIR}`
                // (BIN off 205727901) where `o=u9e(hookInput)={sessionId:session_id,
                // effortLevel:effort?.level, source:"harness"}` (BIN off 199137330).
                // `Uot` emits, in order:
                //   LINGXI="1"                       (always)
                //   LINGXI_SESSION_ID=sessionId     (always)
                //   LINGXI_CHILD_SESSION="1"        (always)
                //   AI_AGENT=Mer("agent")                ONLY when source==="agent"
                //   LINGXI_EFFORT=effortLevel            ONLY when effortLevel set
                //   TRACEPARENT=<otel>                   ONLY when Evt() (OTel on)
                // For hooks `source==="harness"`, so `AI_AGENT` is NEVER emitted on a
                // hook child — distinct from the Bash spawn (`source:"agent"`, which
                // DOES set AI_AGENT). We mirror exactly: set LINGXI /
                // LINGXI_SESSION_ID / LINGXI_CHILD_SESSION unconditionally,
                // LINGXI_EFFORT only when `ctx.effort` carries a level, and we do NOT
                // set AI_AGENT. `TRACEPARENT` is forwarded only when the composition
                // root captured a concrete turn trace context into
                // `HookContext.trace_context`; absent that wiring it stays omitted,
                // matching the current gate-off behavior. Spread BEFORE
                // LINGXI_PROJECT_DIR so the engine project dir still wins (no key
                // overlap, so order is cosmetic, but it tracks the binary's spread
                // position).
                child_env.insert("LINGXI".to_string(), "1".to_string());
                child_env.insert("LINGXI_SESSION_ID".to_string(), ctx.session_id.to_string());
                child_env.insert("LINGXI_CHILD_SESSION".to_string(), "1".to_string());
                if let Some(effort) = &ctx.effort {
                    child_env.insert("LINGXI_EFFORT".to_string(), effort.level.clone());
                }
                let project_dir = ctx.project_dir.clone().unwrap_or_else(|| ctx.cwd.clone());
                let project_dir_str = project_dir.to_string_lossy().into_owned();
                child_env.insert("LINGXI_PROJECT_DIR".to_string(), project_dir_str.clone());
                if let Some(trace_context) = ctx.trace_context.as_ref() {
                    child_env.insert("TRACEPARENT".to_string(), trace_context.traceparent.clone());
                }
                // #43: COLUMNS/LINES from the controlling-terminal size. claude
                // reads `{columns:L,rows:D}=process.stdout` then
                // `if(L)P.COLUMNS=String(L);if(D)P.LINES=String(D)` (BIN off
                // 205727903) — set ONLY when truthy (the `if(L)`/`if(D)` falsy
                // guard). The size is threaded in via `HookContext` from the
                // engine's stdout (the crate has no TTY of its own); `None` /
                // `0` matches a non-TTY `process.stdout.columns === undefined`.
                if let Some(cols) = ctx.terminal_columns.filter(|c| *c != 0) {
                    child_env.insert("COLUMNS".to_string(), cols.to_string());
                }
                if let Some(rows) = ctx.terminal_rows.filter(|r| *r != 0) {
                    child_env.insert("LINES".to_string(), rows.to_string());
                }
                // #43: literal `${LINGXI_PROJECT_DIR}` token substitution in the
                // command AND every arg (claude-code `_e` mapper, BIN off
                // 205727901: `if(!fe.includes("${"))return fe; fe=fe.replaceAll(
                // "${LINGXI_PROJECT_DIR}",()=>S)`, applied as `k=[_e(e.command),
                // e.args.map(_e)]`). A non-shell exec / arg never gets a shell to
                // expand `$LINGXI_PROJECT_DIR`, so the literal `${…}` token must be
                // replaced here. `${LINGXI_PLUGIN_ROOT}` / `${LINGXI_PLUGIN_DATA}`
                // need plugin scope (not on `HookExecutor::Command`) and are a
                // documented residual — a string carrying only those tokens passes
                // through unchanged, matching claude when no plugin scope is bound.
                // SH-06 — `shell` selector (oracle 2.1.238 @ 296948400 spawn
                // head): `v = e.shell ?? Otr()`, `w = v === "powershell"`,
                // `C = e.args !== void 0`. The exec form (`C`) never consults
                // the selector.
                let is_exec_form = !args.is_empty();
                let effective_shell = shell.unwrap_or_else(default_hook_shell);
                let want_powershell =
                    !is_exec_form && effective_shell == crate::definition::HookShell::Powershell;
                // `if(!C&&w){R=o9T(R); if(/\$CLAUDE_PROJECT_DIR\b/.test(R)) warn}`
                // — the PowerShell env-var rewrite runs on the RAW command,
                // BEFORE any `${…}` interpolation, so `${LINGXI_PROJECT_DIR}`
                // becomes `${env:LINGXI_PROJECT_DIR}` and is deliberately NOT
                // substituted literally afterwards (PowerShell reads it from the
                // child env, exactly as upstream intends).
                // `e.command` verbatim — the spelling upstream quotes in both
                // shell-resolution errors (`Hook "<cmd>" has shell: …`).
                let original_command = command.clone();
                let mut base_command = command.clone();
                if want_powershell {
                    if references_bare_project_dir_var(&base_command) {
                        tracing::warn!(
                            "PowerShell hook command references $LINGXI_PROJECT_DIR, which \
                             PowerShell reads as an undefined variable ($null). Use \
                             $env:LINGXI_PROJECT_DIR or ${{LINGXI_PROJECT_DIR}} instead. \
                             Command: {base_command}"
                        );
                    }
                    base_command = powershell_env_token_rewrite(&base_command);
                }
                let command = substitute_project_dir(&base_command, &project_dir_str);
                let args: Vec<String> = args
                    .iter()
                    .map(|a| substitute_project_dir(a, &project_dir_str))
                    .collect();
                // OBS-1 — a `command` hook with NO explicit argv is a SHELL
                // STRING upstream, not a bare exec. claude-code 2.1.238 spawns
                // it (binary @296948400) as
                //   spawn(M, [], {env, cwd, shell: He, detached, windowsHide:!0})
                // where `He = S ? <git-bash path> : !0` — so on POSIX it is
                // literally `shell: true`, i.e. Node runs `/bin/sh -c <M>`.
                // The exec form is a SEPARATE upstream branch, `if(I)
                // spawn(I[0], I[1], …)`, taken only when an argv is supplied.
                //
                // The port used to hand `{command: "./fmt.sh --all", args: []}`
                // straight to `Command::new(command)`, so every hook carrying
                // arguments, a pipe, a redirect or `&&` died with ENOENT. No
                // test caught it because the fixtures only ever PARSE such
                // hooks. The manual `${LINGXI_PROJECT_DIR}` substitution above
                // is a symptom of this same missing shell.
                //
                // `/bin/sh` rather than bash is deliberate twice over: it is
                // what Node's `shell: true` uses on POSIX, AND
                // `is_bash_provider_shell` matches on "bash"/"zsh", so `/bin/sh`
                // keeps a hook child's `SHELL` REMOVED — which is the contract a
                // `source:"harness"` child requires.
                //
                // RESIDUAL (Windows): upstream resolves Git Bash and THROWS when
                // it is absent. The port has no Git-Bash discovery, so Windows
                // keeps the bare-exec behaviour until that lands.
                //
                // SH-06 — the THREE upstream spawn branches, in upstream order:
                //   1. `if(I) spawn(I[0], I[1], …)`          — exec form, no shell
                //   2. `else if(v==="powershell") spawn(bfe(), THn(M), …)`
                //   3. `else spawn(M, [], {shell: He})`      — bash / `/bin/sh -c`
                let (command, args) = if is_exec_form {
                    (command, args)
                } else if want_powershell {
                    // `let Pe=await bfe(); if(!Pe) throw Error(…)`.
                    let Some(exe) = resolve_powershell_executable() else {
                        return map_command_output(
                            hook,
                            Err(ProcessError::Io(powershell_missing_error(
                                &original_command,
                            ))),
                            expected_event,
                        )
                        .0;
                    };
                    // `THn(e) = [...Bfa(), "-Command", e]`.
                    let mut ps_args = powershell_base_args();
                    ps_args.push("-Command".to_string());
                    ps_args.push(command);
                    (exe, ps_args)
                } else if cfg!(windows) {
                    // `let Pe=S?$_e():null; if(S&&!Pe) throw Error(…)` — Windows
                    // bash needs Git Bash. The port has no Git-Bash discovery, so
                    // this keeps the pre-existing bare-exec fallback rather than
                    // synthesizing a refusal upstream would not have raised on a
                    // machine where Git Bash IS installed.
                    (command, args)
                } else {
                    ("/bin/sh".to_string(), vec!["-c".to_string(), command])
                };
                // claude-code writes `jsonStringify(hookInput) + '\n'` to the
                // child's stdin then closes it (`hooks.ts:1006`/`1210`). The
                // trailing newline is load-bearing: a bash `read -r line`
                // returns exit 1 on EOF-before-delimiter without it.
                let pcmd = ProcessCommand {
                    command,
                    args,
                    cwd: resolve_hook_command_cwd(cwd.as_ref(), ctx),
                    env: child_env,
                    timeout: Some(effective_timeout),
                    stdin: Some(format!("{body}\n")),
                };
                // Hooks require workspace trust upstream (claude-code
                // `shouldSkipHookDueToTrust`), so an audited bypass is the
                // parity-honest construction here.
                let sandboxed = sandbox.bypass_with_audit(pcmd, "hook_command");
                // SH-07: start the `hook_progress` poll (claude-code `tWi`)
                // BEFORE the child is spawned, exactly where upstream attaches
                // its `stdout`/`stderr` `data` listeners, and hand the runner a
                // live observer so the accumulator actually fills.
                let progress_frames = self
                    .begin_hook_progress_frames(hook, &format!("{:?}", event.event_type()))
                    .await;
                let live_observer: Option<Arc<dyn platform_api::HookOutputObserver>> =
                    progress_frames.as_ref().map(|frames| {
                        Arc::new(HookLiveOutputObserver {
                            buf: frames.buf.clone(),
                        }) as Arc<dyn platform_api::HookOutputObserver>
                    });
                // Runtime `{"async":true}` first-line detection (claude-code
                // `hooks.ts:1117-1166`): a hook whose first stdout line is that
                // marker is backgrounded and contributes no synchronous decision.
                // A runner without a streaming implementation reports `Completed`
                // for every hook (the default trait method), so non-async hooks —
                // i.e. every hook that does not print the marker — behave exactly
                // as the buffered path did.
                let default_async_timeout = hook.async_timeout.unwrap_or_else(|| {
                    Duration::from_millis(crate::async_registry::DEFAULT_ASYNC_HOOK_TIMEOUT_MS)
                });
                let run_started = std::time::Instant::now();
                let (result, timed_out) = match process
                    .run_hook_with_async_detection_observed(
                        &sandboxed,
                        default_async_timeout,
                        live_observer,
                    )
                    .await
                {
                    Ok(platform_api::HookRunOutcome::Backgrounded {
                        async_timeout,
                        output,
                    }) => {
                        // P2-09 runtime-marker fold-back (claude-code
                        // `registerPendingAsyncHook`): the hook printed
                        // `{"async":true}` as its first stdout line, so it
                        // contributes NO synchronous decision this turn. Its
                        // eventual (post-marker) output is registered with the
                        // async registry so it re-injects as an
                        // `async_hook_response` on a later turn — the completion
                        // publishes to the registry's `completion_tx`, mapped
                        // through the same command-hook contract as a foreground
                        // hook (`map_command_output`).
                        let mut registered = false;
                        if let (Some(output_rx), Some(registry)) = (output, &self.async_registry) {
                            let hook_owned = hook.clone();
                            let hook_id = hook.id;
                            let completion = attachment_completion(
                                self.attachment_sink.clone(),
                                hook_owned.clone(),
                                attachment_identity(event),
                                run_started,
                                self.hook_observer.clone(),
                                progress_id.map(str::to_owned),
                            );
                            let work: HookWork = Box::pin(async move {
                                let out = output_rx.await.unwrap_or_else(|_| {
                                    mobile_linux_api::ProcessOutput {
                                        stdout: String::new(),
                                        stderr: "async hook output channel closed".to_string(),
                                        exit_code: -1,
                                        timed_out: true,
                                    }
                                });
                                map_command_output(&hook_owned, Ok(out), expected_event).0
                            });
                            // Bound the registration by the SAME async timeout the
                            // runner used to bound the child: on normal completion
                            // the (biased) `work` arm wins and publishes the mapped
                            // output; on overrun the registry publishes a timeout
                            // result, matching claude's `asyncTimeout` semantics.
                            match registry
                                .spawn_with_completion_and_rewake(
                                    hook_id,
                                    Some(async_timeout),
                                    work,
                                    completion,
                                    hook.async_rewake
                                        .then(|| hook.rewake_message.clone().unwrap_or_default()),
                                )
                                .await
                            {
                                Ok(_) => registered = true,
                                Err(e) => {
                                    tracing::warn!(
                                        hook_id = %hook_id,
                                        error = %e,
                                        "failed to register async-marker hook fold-back",
                                    );
                                }
                            }
                        }
                        // No synchronous decision — an async-marker hook never
                        // gates the turn.
                        (
                            HookResult {
                                outcome: HookOutcome::Success,
                                stdout: String::new(),
                                stderr: String::new(),
                                exit_code: None,
                                response: registered.then(|| HookResponse {
                                    async_backgrounded: true,
                                    ..HookResponse::default()
                                }),
                            },
                            false,
                        )
                    }
                    Ok(platform_api::HookRunOutcome::Completed(output)) => {
                        map_command_output(hook, Ok(output), expected_event)
                    }
                    Err(e) => map_command_output(hook, Err(e), expected_event),
                };
                // `()=>clearInterval(r)` — stop the poll once the child is done.
                self.finish_hook_progress_frames(progress_frames).await;
                if timed_out {
                    emit_command_timeout(hook, effective_timeout);
                }
                result
            }
            HookExecutor::Agent {
                agent_type,
                prompt,
                model,
            } => {
                let Some((expected_event, body)) = build_envelope_body(event, ctx) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: Agent arm only supports PreToolUse / PostToolUse",
                            hook.id
                        ),
                        exit_code: None,
                        response: None,
                    };
                };
                let effective_timeout = hook
                    .timeout
                    .unwrap_or(Duration::from_millis(HOOK_AGENT_TIMEOUT_MS));
                let exec = AgentExecutor {
                    spawner: self.agent_spawner.clone(),
                    timeout: effective_timeout,
                };
                let outcome = exec
                    .execute(
                        hook,
                        agent_type,
                        prompt,
                        &body,
                        expected_event,
                        ctx.inherit.clone(),
                        model.as_deref(),
                    )
                    .await;
                emit_agent_signal(hook, &outcome.signal, effective_timeout);
                outcome.result
            }
            HookExecutor::Prompt {
                prompt,
                model,
                continue_on_block,
            } => {
                let Some((_expected_event, body)) = build_envelope_body(event, ctx) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: Prompt arm does not support {:?}",
                            hook.id,
                            event.event_type()
                        ),
                        exit_code: None,
                        response: None,
                    };
                };
                // `execPromptHook.ts:55`: `hook.timeout * 1000` (seconds → ms),
                // else the 30 s default. Our `hook.timeout` is already a
                // `Duration`; a zero/absent timeout defers to the default.
                let effective_timeout = match hook.timeout {
                    Some(t) if !t.is_zero() => t,
                    _ => Duration::from_millis(HOOK_PROMPT_TIMEOUT_MS),
                };
                let exec = PromptExecutor {
                    transcript: ctx.prompt_transcript.clone(),
                    runner: self.prompt_runner.clone(),
                    timeout: effective_timeout,
                };
                let outcome = exec
                    .execute(hook, prompt, model.as_deref(), *continue_on_block, &body)
                    .await;
                emit_prompt_signal(hook, &outcome.signal, effective_timeout);
                outcome.result
            }
            HookExecutor::McpTool { server, tool, .. } => {
                let Some(invoker) = &self.mcp_invoker else {
                    tracing::warn!(
                        hook_id = %hook.id,
                        server = %server,
                        tool = %tool,
                        "mcp_tool hook loaded but not executed: no MCP invoker is wired into the hooks crate",
                    );
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!("Hook {} failed: mcp_tool executor not wired", hook.id),
                        exit_code: None,
                        response: None,
                    };
                };
                let Some((expected_event, body)) = build_envelope_body(event, ctx) else {
                    return HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: format!(
                            "Hook {} failed: mcp_tool arm does not support {:?}",
                            hook.id,
                            event.event_type()
                        ),
                        exit_code: None,
                        response: None,
                    };
                };
                let body_value: Value = match serde_json::from_str(&body) {
                    Ok(value) => value,
                    Err(error) => {
                        return HookResult {
                            outcome: HookOutcome::Error,
                            stdout: String::new(),
                            stderr: format!(
                                "Hook {} failed: invalid hook payload: {error}",
                                hook.id
                            ),
                            exit_code: None,
                            response: None,
                        };
                    }
                };
                let HookExecutor::McpTool { input, .. } = &hook.executor else {
                    unreachable!("matched McpTool executor");
                };
                let effective_timeout = match hook.timeout {
                    Some(timeout) if !timeout.is_zero() => timeout,
                    _ => Duration::from_millis(HOOK_COMMAND_TIMEOUT_MS),
                };
                match invoker
                    .invoke(HookMcpInvocation {
                        server: server.clone(),
                        tool: tool.clone(),
                        input: expand_mcp_hook_input_map(input, &body_value),
                        timeout: effective_timeout,
                    })
                    .await
                {
                    HookMcpInvocationResult::Success { text_content } => {
                        let (result, _) = map_text_hook_output(
                            hook,
                            join_mcp_text_content(&text_content),
                            String::new(),
                            0,
                            expected_event,
                        );
                        result
                    }
                    HookMcpInvocationResult::Error {
                        text_content,
                        message,
                    } => HookResult {
                        outcome: HookOutcome::Error,
                        stdout: join_mcp_text_content(&text_content),
                        stderr: message,
                        exit_code: Some(1),
                        response: None,
                    },
                    HookMcpInvocationResult::NotConnected { message } => HookResult {
                        outcome: HookOutcome::Error,
                        stdout: String::new(),
                        stderr: message,
                        exit_code: Some(1),
                        response: None,
                    },
                    HookMcpInvocationResult::Timeout { text_content } => {
                        emit_mcp_timeout(hook, effective_timeout);
                        HookResult {
                            outcome: HookOutcome::Timeout,
                            stdout: join_mcp_text_content(&text_content),
                            stderr: format!("Hook {} failed: mcp_tool timed out", hook.id),
                            exit_code: None,
                            response: None,
                        }
                    }
                }
            }
        }
    }
}

impl HookExecutorImpl {
    /// #41 runner-head gate (`h$`/`cH`, BIN off 195521423 / 205755512). When
    /// the `policySettings.disableAllHooks` managed setting is active, every
    /// `execute*` path returns the empty (default) aggregate WITHOUT dispatching
    /// any hook, after logging the binary's skip line
    /// `Skipping hooks for {event[:match_query]} due to 'disableAllHooks' managed
    /// setting` (the binary's label is `event:matchQuery` when a `matchQuery` is
    /// present, else just `event`). Returns `Some(default_aggregate)` when gated,
    /// `None` otherwise.
    fn policy_disable_gate(&self, event: &HookEvent) -> Option<AggregateHookResult> {
        if !self.policy_disable_all_hooks {
            return None;
        }
        let event_name = format!("{:?}", event.event_type());
        let label = match Self::runner_match_query(event) {
            Some(q) => format!("{event_name}:{q}"),
            None => event_name,
        };
        tracing::info!("Skipping hooks for {label} due to 'disableAllHooks' managed setting");
        Some(AggregateHookResult::default())
    }

    /// The `matchQuery` the binary's `cH` runner is invoked with for `event`
    /// (the tool/model name for tool/permission/model-switch events, else
    /// `None`) — used only to
    /// format the #41 skip-log label. Mirrors `HookRegistry::match_query_for`.
    fn runner_match_query(event: &HookEvent) -> Option<String> {
        match event {
            HookEvent::PreToolUse { tool_name, .. }
            | HookEvent::PostToolUse { tool_name, .. }
            | HookEvent::PostToolUseFailure { tool_name, .. }
            | HookEvent::PermissionRequest { tool_name, .. }
            | HookEvent::PermissionDenied { tool_name, .. } => Some(tool_name.clone()),
            HookEvent::PreModelSwitch { to_model, .. }
            | HookEvent::PostModelSwitch { to_model, .. } => Some(to_model.clone()),
            _ => None,
        }
    }

    /// `hook_event` is the `Debug` event name (`f` in the oracle's aggregation
    /// loop) — needed by the SH-01 `classifierContext` log line, which upstream
    /// formats as ``Hook ${f} (${dJ(z.hook)}) provided classifierContext …``.
    fn merge(
        agg: &mut AggregateHookResult,
        hook: &HookDefinition,
        r: HookResult,
        hook_event: &str,
        eval_confined: bool,
    ) {
        let mut r = r;
        // PreModelSwitch is a gate: execution failures before a hook can
        // answer must stop the switch. A command's explicit non-2 exit remains
        // advisory (the command mapper preserves its exit code), matching the
        // upstream distinction between a failed invocation and a hook choosing
        // an arbitrary non-zero status. PostModelSwitch is already after the
        // mutation, so a missing/failed response never blocks anything.
        if hook_event == "PreModelSwitch"
            && r.response.is_none()
            && r.exit_code.is_none()
            && matches!(
                r.outcome,
                HookOutcome::Error | HookOutcome::Timeout | HookOutcome::Cancelled
            )
        {
            let reason = if r.stderr.is_empty() {
                format!("PreModelSwitch hook {} failed before answering", hook.id)
            } else {
                r.stderr.clone()
            };
            r.response = Some(HookResponse {
                decision: Some(HookDecision::Block),
                reason: Some(reason),
                ..HookResponse::default()
            });
        }
        // PARITY 2.1.263 `H_n(response, label)` — a CONFINED eval session takes
        // permission grants only from its command line, so a hook's ALLOW is
        // dropped before it can reach the aggregate:
        //
        // ```js
        // function H_n(e,t){
        //   if(!YYe()) return e;                       // CLAUDE_CODE_EVAL_CONFINED
        //   if(e.permissionBehavior==="allow"){ n(`${t} permissionDecision=allow ignored: …`); e.permissionBehavior=void 0 }
        //   if(e.permissionRequestResult?.behavior==="allow"){ n(`${t} PermissionRequest allow ignored: …`); e.permissionRequestResult=void 0 }
        //   return e }
        // ```
        //
        // Only the ALLOW channels are suppressed — a hook may still block or
        // ask, which is the whole point of a confined harness run.
        if eval_confined {
            if let Some(resp) = &mut r.response {
                let label = hook_event;
                if resp.decision == Some(HookDecision::Approve) {
                    tracing::info!(
                        target: "hooks",
                        "{label} permissionDecision=allow ignored: a confined session takes grants only from its command line"
                    );
                    resp.decision = None;
                }
                if matches!(
                    resp.permission_request_result,
                    Some(crate::response::PermissionRequestResult::Allow { .. })
                ) {
                    tracing::info!(
                        target: "hooks",
                        "{label} PermissionRequest allow ignored: a confined session takes grants only from its command line"
                    );
                    resp.permission_request_result = None;
                }
            }
        }
        if let Some(resp) = &r.response {
            // #45(b): claude-code's `cH` runner dispatches EVERY matched hook
            // (`c.map(async …)` + await-all, BIN off 205755512 — no break) then
            // folds the verdict via `ctt(e)=e.some(t=>t.blocked)`: any hook's
            // block wins, regardless of order. Now that the per-event loops no
            // longer short-circuit on the first `Block`, `Block` must be STICKY
            // here so a later non-`Block` hook can't overwrite an earlier block
            // (the OR-fold equivalent of `some(blocked)`), and the block `reason`
            // freezes at the FIRST blocker. Every OTHER channel
            // (systemMessage/additionalContext/updatedInput/…) still accumulates
            // or last-wins exactly as before, so later hooks' side effects — which
            // the old early `break` silently dropped — are now preserved.
            let already_blocked =
                matches!(agg.decision, Some(crate::response::HookDecision::Block));
            // PostModelSwitch cannot gate a switch that has already happened;
            // retain its response in `all_results` but keep the aggregate
            // decision channel empty for best-effort callers.
            let decision = (hook_event != "PostModelSwitch")
                .then_some(resp.decision)
                .flatten();
            if decision.is_some() && !already_blocked {
                agg.decision = decision;
                agg.hook_source = Some(hook.source);
            }
            // Freeze the block reason at the first blocker: once blocked, a later
            // hook's `reason` no longer overwrites the aggregate one.
            if let Some(reason) = &resp.reason {
                if !already_blocked {
                    agg.reason = Some(reason.clone());
                }
            }
            // O2: freeze the blocking hook's `command` the SAME way, so
            // `reason` + `block_command` always describe the one hook that
            // blocked — they are the two halves of a single `blockingError`
            // object (BIN off 237775430) and must not come from different hooks.
            // The exit-2 arm supplies `iSe`; every other blocking arm (JSON
            // `decision:"block"`, PreToolUse `permissionDecision:"deny"`) is
            // reached through `Tfn({command: ee})`, i.e. `qq`.
            if !already_blocked && matches!(decision, Some(crate::response::HookDecision::Block)) {
                agg.block_command = Some(
                    resp.block_command
                        .clone()
                        .unwrap_or_else(|| attachment::attachment_command(hook)),
                );
            }
            if let Some(input) = &resp.updated_input {
                agg.modified_input = Some(input.clone());
            }
            // PermissionRequest carries an event-specific decision object in
            // addition to the normalized HookDecision. Preserve the latest raw
            // result and its permission-rule array; the block decision above
            // remains sticky while these side effects stay available to the
            // orchestrator's allow arm.
            if let Some(permission_result) = &resp.permission_request_result {
                if !already_blocked {
                    agg.permission_request_result = Some(permission_result.clone());
                }
                if !already_blocked {
                    if let PermissionRequestResult::Allow {
                        updated_permissions: Some(updates),
                        ..
                    } = permission_result
                    {
                        agg.permission_updates = updates.clone();
                    }
                }
            }
            if !already_blocked {
                if let Some(updates) = &resp.updated_permissions {
                    agg.permission_updates = updates.clone();
                }
            }
            if resp.interrupt == Some(true) {
                agg.interrupt = true;
            }
            if let Some(msg) = &resp.system_message {
                agg.system_messages.push(msg.clone());
            }
            // `additionalContext` is kept on its OWN aggregate channel, distinct
            // from `system_messages`: only `additional_contexts` reaches the
            // model (claude-code `hook_additional_context`, `messages.ts:4117`),
            // whereas `system_messages` is transcript-facing only (`:4258`). It is
            // injected as its own <system-reminder> message, not folded into the
            // tool_result content (claude-code `toolExecution.ts:845`).
            if let Some(ctx) = &resp.additional_context {
                agg.additional_contexts.push(ctx.clone());
            }
            // B4: OR-fold the preventContinuation (`continue:false`) signal so
            // a single lifecycle hook can terminate the agent loop even when
            // earlier hooks did not. Independent of `decision`.
            if resp.prevent_continuation {
                agg.prevent_continuation = true;
            }
            // Elicitation answer: keep the latest non-empty response, mirroring
            // `executeElicitationHooks`'s loop (`utils/hooks.ts:4512-4520`)
            // which overwrites `elicitationResponse` with each parsed result.
            if let Some(er) = &resp.elicitation_response {
                agg.elicitation_response = Some(er.clone());
            }
            // PostToolUse `updatedMCPToolOutput`: keep the latest replacement
            // any folded hook returned, mirroring TS's per-hook assignment
            // (`result.updatedMCPToolOutput = …`, `utils/hooks.ts:647`). The
            // orchestrator applies it only for MCP tools (`isMcpTool(tool)`).
            if let Some(out) = &resp.updated_mcp_tool_output {
                agg.updated_mcp_tool_output = Some(out.clone());
            }
            // PostToolUse `updatedToolOutput` (all-tools, #38): keep the latest
            // replacement any folded hook returned (claude-code keeps the most
            // recent — BIN off 205724076). `!== void 0` semantics are preserved
            // by the `Option<Option<Value>>` shape: the outer `Some` means a
            // hook set the key (even to `null`), so we fold whenever it is
            // `Some`. The orchestrator applies it for ALL tools (no isMcp gate).
            if let Some(out) = &resp.updated_tool_output {
                agg.updated_tool_output = Some(out.clone());
            }
            // SH-01 `classifierContext` (oracle 2.1.238 @ 296974134):
            //
            //   if(z.classifierContext){ let U=wo(z.classifierContext,Pfr);
            //     T(`Hook ${f} (${dJ(z.hook)}) provided classifierContext (${U.length} chars after cap)`),
            //     M.classifierContextChars+=U.length, G(z.hook,"classifierContextChars",U.length),
            //     yield{pairedRewrite: …, classifierContexts:[{value:U,hostPrincipal:…}]} }
            //
            // The cap runs HERE (not at parse) so the per-hook truncation and
            // the shared character budget stay together, exactly as upstream.
            if let Some(raw) = &resp.classifier_context {
                let capped = crate::response::truncate_utf16(
                    raw,
                    crate::response::CLASSIFIER_CONTEXT_CAP_UTF16,
                );
                // `U.length` is UTF-16 code units, like the cap itself.
                let chars = capped.encode_utf16().count();
                tracing::debug!(
                    "Hook {} ({}) provided classifierContext ({chars} chars after cap)",
                    hook_event,
                    attachment::attachment_command(hook),
                );
                agg.classifier_context_chars += chars;
                // `pairedRewrite` describes THIS hook's own rewrite, so it is
                // computed from `resp`, never from the aggregate.
                agg.paired_rewrite = Some(if resp.updated_tool_output.is_some() {
                    crate::response::PairedRewrite::Direct
                } else if resp.updated_mcp_tool_output.is_some() {
                    crate::response::PairedRewrite::LegacyMcp
                } else {
                    crate::response::PairedRewrite::None
                });
                agg.classifier_contexts
                    .push(crate::response::ClassifierHostContext {
                        value: capped,
                        // `hostPrincipal` is true only for an in-process
                        // `callback` hook owned by neither a plugin nor a skill;
                        // the port has no `callback` executor, so it is false for
                        // every hook type the loader can build.
                        host_principal: false,
                    });
            }
            // PermissionDenied `retry`: OR-fold so a single hook saying
            // `retry: true` flips the aggregate, mirroring TS's
            // `if (result.retry) hookSaysRetry = true` (`toolExecution.ts:1090`).
            // A `Some(false)` / `None` leaves it untouched.
            if resp.retry == Some(true) {
                agg.retry = true;
            }
            // #40 `terminalSequence`: keep the latest a folded hook returned
            // (claude-code applies `szn` per hook result). The consumer (TUI
            // terminal writer) runs the allowlist validator + emit on apply.
            if let Some(ts) = &resp.terminal_sequence {
                agg.terminal_sequence = Some(ts.clone());
            }
            // `sessionTitle` (UserPromptSubmit output, BIN off 201754804):
            // keep the latest — mirrors TS's `applyHookSessionTitle` last-wins
            // assignment. `None` for all non-`UserPromptSubmit` hooks.
            if let Some(title) = &resp.session_title {
                agg.session_title = Some(title.clone());
            }
            // `suppressOriginalPrompt` (UserPromptSubmit output): OR-fold so a
            // single hook setting it flips the aggregate (a later hook returning
            // `false` doesn't undo a prior `true`).
            if resp.suppress_original_prompt {
                agg.suppress_original_prompt = true;
            }
            // `displayContent` (MessageDisplay output, BIN off 201757586):
            // keep the latest a folded hook returned — mirrors TS last-wins.
            if let Some(dc) = &resp.display_content {
                agg.display_content = Some(dc.clone());
            }
            // `watchPaths` (FileChanged / CwdChanged output): accumulate every
            // hook's entries in execution order — claude-code's `v3r` / `E3r`
            // return `{ ..., watchPaths }` as the concatenation of each fired
            // hook's `hookSpecificOutput.watchPaths`. The file-changed watcher
            // restarts over the union when the folded set is non-empty.
            if let Some(paths) = &resp.watch_paths {
                agg.watch_paths.extend(paths.iter().cloned());
            }
            // `initialUserMessage` (SessionStart output): keep the latest —
            // `if(p.initialUserMessage)$os=p.initialUserMessage` last-wins.
            if let Some(m) = &resp.initial_user_message {
                agg.initial_user_message = Some(m.clone());
            }
            // `reloadSkills` (SessionStart output): OR-fold — `if(p.reloadSkills)u=!0`.
            if resp.reload_skills == Some(true) {
                agg.reload_skills = true;
            }
            agg.attachments.extend(resp.attachments.clone());
        }
        agg.all_results.push((hook.id, r));
    }
}

/// SH-01 — hand every folded `classifierContext` to the auto-mode permission
/// classifier's host-context store.
///
/// This is the wire that makes the whole `classifierContext` path reachable:
/// the hooks layer parses + caps the value, and THIS call is what puts it where
/// `permission::policy_gate`'s auto-mode arm reads it back
/// (`classify_tool_call_with_host_context`). Records published from a live hook
/// dispatch are `live = true` — they were attached during this session — which
/// is the only provenance that upstream lets carry user intent at all.
///
/// A no-op (not even a lock acquisition) when no hook supplied a context, which
/// is every dispatch in a default install.
fn publish_classifier_host_contexts(agg: &AggregateHookResult, identity: &HookAttachmentIdentity) {
    if agg.classifier_contexts.is_empty() {
        return;
    }
    permission::host_context::store().publish(
        &identity.tool_use_id,
        agg.classifier_contexts
            .iter()
            .map(|c| (c.value.clone(), c.host_principal)),
        true,
    );
}

/// SH-07 — `tWi`'s `e.intervalMs ?? 1000` poll cadence for `hook_progress`
/// frames (oracle 2.1.238 @ 296463298). No call site supplies `intervalMs`, so
/// 1 s is the only value the binary ever uses.
pub const HOOK_PROGRESS_INTERVAL_MS: u64 = 1000;

/// SH-07 — live accumulator for one hook child's output, the port's stand-in for
/// upstream's `te` / `ee` / `re` closure locals (oracle @ 296950289):
/// `ne=(Pe)=>{ee+=Pe,re+=Pe}` on stderr and `X=(Pe)=>{te+=Pe,re+=Pe}` on stdout.
///
/// [`Self::output`] is therefore the ARRIVAL-ORDERED interleaving of both pipes,
/// not `stdout + stderr` — which matters because it is the value the poll's
/// change detection compares.
#[derive(Default)]
struct HookLiveOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output: Vec<u8>,
}

impl HookLiveOutput {
    /// Decode the three buffers once, over whole buffers — a per-chunk decode
    /// would corrupt a multi-byte sequence split across a read boundary.
    fn snapshot(&self) -> (String, String, String) {
        (
            String::from_utf8_lossy(&self.stdout).into_owned(),
            String::from_utf8_lossy(&self.stderr).into_owned(),
            String::from_utf8_lossy(&self.output).into_owned(),
        )
    }
}

/// SH-07 — the [`platform_api::HookOutputObserver`] the runner pushes chunks into.
struct HookLiveOutputObserver {
    buf: Arc<std::sync::Mutex<HookLiveOutput>>,
}

#[async_trait]
impl platform_api::HookOutputObserver for HookLiveOutputObserver {
    async fn on_chunk(&self, stdout_delta: &[u8], stderr_delta: &[u8]) {
        let mut live = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        if !stdout_delta.is_empty() {
            live.stdout.extend_from_slice(stdout_delta);
            live.output.extend_from_slice(stdout_delta);
        }
        if !stderr_delta.is_empty() {
            live.stderr.extend_from_slice(stderr_delta);
            live.output.extend_from_slice(stderr_delta);
        }
    }
}

/// SH-07 — the handle `tWi` returns: the shared accumulator plus everything
/// needed to run `clearInterval` on it.
struct HookProgressFrames {
    buf: Arc<std::sync::Mutex<HookLiveOutput>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<platform_api::BackgroundTaskHandle>,
}

/// SH-06 — `Otr()` (oracle 2.1.238 @ 284437507):
/// `function Otr(){return Sh()?"bash":"powershell"}` where
/// `function Sh(){if(Wt()!=="windows")return!0;return $_e()!==null}` — i.e. the
/// implicit default is `bash` everywhere except Windows-without-Git-Bash, where
/// it is `powershell`.
///
/// The port has no Git-Bash discovery of its own inside the hooks crate, so on
/// Windows it reports `Powershell` (upstream's no-Git-Bash arm). That is the
/// SAFE side of the fork: the bash arm on Windows would immediately throw
/// upstream's `requires bash but Git Bash was not found` error, whereas the
/// PowerShell arm resolves a real interpreter. Non-Windows is unconditional
/// `Bash`, byte-identical to `Otr()`.
#[must_use]
pub fn default_hook_shell() -> crate::definition::HookShell {
    if cfg!(windows) {
        crate::definition::HookShell::Powershell
    } else {
        crate::definition::HookShell::Bash
    }
}

/// SH-06 — `Bfa()` (oracle 2.1.238 @ 284461074):
/// `let e=["-NoProfile","-NonInteractive"]; if(!V.CLAUDE_CODE_POWERSHELL_RESPECT_EXECUTION_POLICY)
///  e.push("-ExecutionPolicy","Bypass"); return e`.
///
/// The env guard is raw JS truthiness on the env value (any non-empty string
/// suppresses the `-ExecutionPolicy Bypass` pair), NOT the strict
/// `isEnvTruthy` predicate — mirrored here with a non-empty check.
///
/// Both spellings are read, `LINGXI_` first then the upstream `CLAUDE_CODE_`
/// name — the same two-spelling convention `tools::shell::bash`'s
/// `LINGXI_GIT_BASH_PATH` / `CLAUDE_CODE_GIT_BASH_PATH` override uses.
#[must_use]
pub fn powershell_base_args() -> Vec<String> {
    let mut args = vec!["-NoProfile".to_string(), "-NonInteractive".to_string()];
    let respect_policy = [
        "LINGXI_POWERSHELL_RESPECT_EXECUTION_POLICY",
        "CLAUDE_CODE_POWERSHELL_RESPECT_EXECUTION_POLICY",
    ]
    .iter()
    .any(|var| std::env::var(var).is_ok_and(|v| !v.is_empty()));
    if !respect_policy {
        args.push("-ExecutionPolicy".to_string());
        args.push("Bypass".to_string());
    }
    args
}

/// SH-06 — `bfe()`/`aBb()` (oracle 2.1.238 @ 284436932 / 284435843), POSIX arm:
/// `await yT("pwsh")` first, then `await yT("powershell")`, else `null`. The
/// Linux `/snap/` workaround and the Windows `ProgramFiles`/`LOCALAPPDATA`/
/// `USERPROFILE` probes are Windows/snap-only residuals the port does not
/// carry; the PATH scan is the branch every POSIX host takes.
#[must_use]
pub fn resolve_powershell_executable() -> Option<String> {
    let path_env = std::env::var_os("PATH")?;
    for name in ["pwsh", "powershell"] {
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
            #[cfg(windows)]
            {
                let exe = dir.join(format!("{name}.exe"));
                if exe.is_file() {
                    return Some(exe.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

/// SH-06 — the byte-locked `shell: "powershell"` resolution failure (oracle
/// 2.1.238 @ 296948400). Upstream `throw`s this; the port maps it through
/// [`map_command_output`]'s I/O arm so the hook reports a non-blocking error the
/// same way a spawn failure does.
#[must_use]
pub fn powershell_missing_error(command: &str) -> String {
    format!(
        "Hook \"{command}\" has shell: 'powershell' but no PowerShell executable \
         (pwsh or powershell) was found on PATH. Install PowerShell, or remove \
         \"shell\": \"powershell\" to use bash."
    )
}

/// SH-06 — `o9T` (oracle 2.1.238 @ 296991060):
/// ```js
/// for(let t of ["CLAUDE_PROJECT_DIR","CLAUDE_PLUGIN_ROOT","CLAUDE_PLUGIN_DATA"])
///   e=e.replaceAll("${"+t+"}",()=>"${env:"+t+"}");
/// ```
/// PowerShell has no `${VAR}` env syntax, so the three host tokens are rewritten
/// to `${env:VAR}` before the command reaches `-Command`.
#[must_use]
pub fn powershell_env_token_rewrite(command: &str) -> String {
    let mut out = command.to_string();
    for name in [
        "LINGXI_PROJECT_DIR",
        "LINGXI_PLUGIN_ROOT",
        "LINGXI_PLUGIN_DATA",
    ] {
        out = out.replace(&format!("${{{name}}}"), &format!("${{env:{name}}}"));
    }
    out
}

/// SH-06 — the `/\$CLAUDE_PROJECT_DIR\b/` probe upstream warns on before
/// spawning a shell-form PowerShell hook: a BARE `$LINGXI_PROJECT_DIR` (no
/// `env:` scope, not the `${…}` form that [`powershell_env_token_rewrite`]
/// already fixed) reads as `$null` inside PowerShell.
#[must_use]
pub fn references_bare_project_dir_var(command: &str) -> bool {
    const VAR: &str = "$LINGXI_PROJECT_DIR";
    let bytes = command.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = command[from..].find(VAR) {
        let at = from + rel;
        let after = at + VAR.len();
        // `\b`: the match must not be followed by another word character.
        let boundary = bytes
            .get(after)
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
        if boundary {
            return true;
        }
        from = after;
    }
    false
}

/// #43: substitute every literal `${LINGXI_PROJECT_DIR}` token in `s` with
/// `project_dir`.
///
/// Byte-faithful port of claude-code's `_e` mapper (BIN off 205727901):
/// `if(!fe.includes("${"))return fe; fe=fe.replaceAll("${LINGXI_PROJECT_DIR}",
/// ()=>S)` — the `${`-presence fast-path guard (a string with no `${` is returned
/// untouched, skipping the scan) and `replaceAll` (EVERY occurrence) semantics.
/// `()=>S` is a replacer FUNCTION in JS, so a literal `$1` / `$&` in the project
/// path is NOT treated as a replacement-pattern special; `str::replace` matches
/// that (it inserts the replacement verbatim).
///
/// The plugin tokens `${LINGXI_PLUGIN_ROOT}` / `${LINGXI_PLUGIN_DATA}` are NOT
/// handled here: they require plugin scope (`pluginRoot` / `pluginData`), which
/// `HookExecutor::Command` does not carry — a documented residual. A string
/// containing only those tokens (and no `${LINGXI_PROJECT_DIR}`) passes through
/// unchanged, matching claude when no plugin scope is bound.
fn substitute_project_dir(s: &str, project_dir: &str) -> String {
    // `if(!fe.includes("${"))return fe` — fast path: no template token at all.
    if !s.contains("${") {
        return s.to_string();
    }
    s.replace("${LINGXI_PROJECT_DIR}", project_dir)
}

fn resolve_hook_command_cwd(configured: Option<&PathBuf>, ctx: &HookContext) -> Option<PathBuf> {
    if let Some(configured) = configured {
        if is_existing_dir(configured) {
            return Some(configured.clone());
        }
        return fallback_hook_command_cwd(ctx);
    }
    if is_existing_dir(&ctx.cwd) {
        return Some(ctx.cwd.clone());
    }
    fallback_hook_command_cwd(ctx)
}

fn fallback_hook_command_cwd(ctx: &HookContext) -> Option<PathBuf> {
    if let Some(project_dir) = ctx
        .project_dir
        .as_ref()
        .filter(|path| is_existing_dir(path))
    {
        return Some(project_dir.clone());
    }
    hook_home_dir()
}

fn hook_home_dir() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("USERPROFILE").map(PathBuf::from),
        match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
            (Some(drive), Some(path)) => Some(PathBuf::from(drive).join(path)),
            _ => None,
        },
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|path| is_existing_dir(path))
}

fn is_existing_dir(path: &Path) -> bool {
    path.is_dir()
}

/// Build the serialized envelope body + `expected_event` marker for an event.
///
/// Returns `None` for event variants the HTTP / Agent arms don't yet support.
/// Tool events (`PreToolUse` / `PostToolUse`, M5-06) plus the lifecycle events
/// `Stop` / `SubagentStop` / `TaskCompleted` / `UserPromptSubmit` /
/// `SessionStart` / `StopFailure` (B1) are serialized here; every other variant
/// still falls through to `None` until its wire schema is ported.
///
/// Where a [`HookEvent`] variant carries fewer fields than the claude-code wire
/// schema (e.g. `Stop` has no `stop_hook_active` / `last_assistant_message`
/// yet, `TaskCompleted` only carries `task_id`), the available fields are
/// populated and the rest defaulted (`false` / `None` / empty string). The
/// missing fields are filled by later B-cluster batches that thread richer
/// context through `HookEvent` / `HookContext`.
fn build_envelope_body(event: &HookEvent, ctx: &HookContext) -> Option<(&'static str, String)> {
    match event {
        HookEvent::PreToolUse {
            tool_name,
            tool_input,
            tool_use_id,
        } => {
            let payload = PreToolUsePayload {
                hook_event_name: HookEventNamePre,
                session_id: ctx.session_id.to_string(),
                transcript_path: ctx.transcript_path.to_string_lossy().into_owned(),
                cwd: ctx.cwd.to_string_lossy().into_owned(),
                prompt_id: ctx.prompt_id.clone(),
                permission_mode: ctx.permission_mode.clone(),
                agent_id: ctx.agent_id.as_ref().map(ToString::to_string),
                agent_type: ctx.agent_type.clone(),
                effort: ctx.effort.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                tool_use_id: tool_use_id.to_string(),
            };
            Some(("PreToolUse", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::PostToolUse {
            tool_name,
            tool_input,
            tool_output,
            tool_use_id,
            duration_ms,
        } => {
            let payload = PostToolUsePayload {
                hook_event_name: HookEventNamePost,
                session_id: ctx.session_id.to_string(),
                transcript_path: ctx.transcript_path.to_string_lossy().into_owned(),
                cwd: ctx.cwd.to_string_lossy().into_owned(),
                prompt_id: ctx.prompt_id.clone(),
                permission_mode: ctx.permission_mode.clone(),
                agent_id: ctx.agent_id.as_ref().map(ToString::to_string),
                agent_type: ctx.agent_type.clone(),
                effort: ctx.effort.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                tool_response: tool_output.clone(),
                tool_use_id: tool_use_id.to_string(),
                duration_ms: *duration_ms,
            };
            Some(("PostToolUse", serde_json::to_string(&payload).ok()?))
        }
        // Lifecycle events (B1) share the `createBaseHookInput` base shape; they
        // are split into a helper to keep this dispatch readable.
        _ => build_lifecycle_envelope_body(event, ctx),
    }
}

/// The `createBaseHookInput` base shape (`utils/hooks.ts:301-328`) extracted
/// from a [`HookContext`], reused by every lifecycle payload.
struct BaseHookFields {
    session_id: String,
    transcript_path: String,
    cwd: String,
    /// `prompt_id` — the UUID correlating a user prompt with every subsequent
    /// hook event until the next prompt. Oracle `createBaseHookInput`
    /// (2.1.238 minified `c_`, BIN off 296935693) emits it between `cwd` and
    /// `permission_mode` as `prompt_id:Vut()??void 0`; `None` omits the key,
    /// faithful to "absent until the first user input of the process
    /// lifetime".
    prompt_id: Option<String>,
    permission_mode: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    /// Active reasoning-effort level (`effort: { level }`), sourced from the
    /// [`HookContext`]. `None` (omitted on the wire) for session-lifecycle
    /// hooks and effort-incapable models — faithful to claude-code's
    /// conditional `effort` spread in `createBaseHookInput`.
    effort: Option<crate::hook_payload::EffortLevel>,
    /// Current session title (binary-confirmed at BIN off 201745825). Threaded
    /// into `UserPromptSubmit` and `SessionStart` payloads; `None` for all
    /// other events (they ignore this field even if populated in ctx).
    session_title: Option<String>,
}

impl BaseHookFields {
    fn from_ctx(ctx: &HookContext) -> Self {
        Self {
            session_id: ctx.session_id.to_string(),
            transcript_path: ctx.transcript_path.to_string_lossy().into_owned(),
            cwd: ctx.cwd.to_string_lossy().into_owned(),
            prompt_id: ctx.prompt_id.clone(),
            permission_mode: ctx.permission_mode.clone(),
            agent_id: ctx.agent_id.as_ref().map(ToString::to_string),
            agent_type: ctx.agent_type.clone(),
            effort: ctx.effort.clone(),
            session_title: ctx.session_title.clone(),
        }
    }
}

/// Serialize the B1 lifecycle events (`Stop` / `SubagentStop` /
/// `TaskCompleted` / `UserPromptSubmit` / `SessionStart` / `StopFailure`) plus
/// the B6 additions (`PostToolUseFailure` / `SessionEnd` / `PreCompact` /
/// `PostCompact` / `Notification` / `PermissionRequest` / `Setup` /
/// `SubagentStart` / `CwdChanged` / `FileChanged` / `WorktreeRemove`).
///
/// Where a [`HookEvent`] variant carries fewer fields than the claude-code wire
/// schema, the available fields are populated and the rest defaulted
/// (`false` / `None` / empty string) — filled by later B-cluster batches.
///
/// The final four events (`ConfigChange` / `InstructionsLoaded` /
/// `Elicitation` / `WorktreeCreate`) — previously deferred because their
/// `HookEvent` variant lacked a field to source a *required* wire value — now
/// carry those fields and serialize faithfully. `PermissionDenied` is likewise
/// ported here (the hook-firing batch extended its variant with `tool_input` /
/// `tool_use_id`). `TaskCreated` is now serialized too (mirroring the
/// `TaskCompleted` arm, fired through the `TaskCreatedFirer` seam). The parity-fix
/// batch then ported `ElicitationResult` ([P0] gap). All 30 `HookEvent` variants
/// are now serializable; the `_ => None` catch-all is a forward-compatibility guard.
#[allow(
    clippy::too_many_lines,
    reason = "per-event payload construction fan-out — splitting hurts readability"
)]
fn build_lifecycle_envelope_body(
    event: &HookEvent,
    ctx: &HookContext,
) -> Option<(&'static str, String)> {
    let b = BaseHookFields::from_ctx(ctx);
    match event {
        HookEvent::Stop { .. } => {
            let payload = StopPayload {
                hook_event_name: HookEventNameStop,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                stop_hook_active: ctx.stop_hook_active,
                last_assistant_message: ctx.last_assistant_message.clone(),
                background_tasks: ctx.background_tasks.clone(),
                session_crons: ctx.session_crons.clone(),
            };
            Some(("Stop", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::SubagentStop {
            agent_id,
            agent_type,
            ..
        } => {
            let payload = SubagentStopPayload {
                hook_event_name: HookEventNameSubagentStop,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                stop_hook_active: ctx.stop_hook_active,
                agent_id: agent_id.to_string(),
                agent_transcript_path: ctx
                    .agent_transcript_path
                    .as_ref()
                    .map_or_else(String::new, |path| path.to_string_lossy().into_owned()),
                // claude `agent_type: a ?? ""` — now carried on the event
                // (mirrors SubagentStart); fall back to the context for older
                // call paths that left the event's `agent_type` empty.
                agent_type: if agent_type.is_empty() {
                    b.agent_type.unwrap_or_default()
                } else {
                    agent_type.clone()
                },
                effort: b.effort,
                last_assistant_message: ctx.last_assistant_message.clone(),
                background_tasks: ctx.background_tasks.clone(),
                session_crons: ctx.session_crons.clone(),
            };
            Some(("SubagentStop", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::TaskCompleted {
            task_id,
            task_subject,
            task_description,
            teammate_name,
            team_name,
            ..
        } => {
            let payload = TaskCompletedPayload {
                hook_event_name: HookEventNameTaskCompleted,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                task_id: task_id.clone(),
                task_subject: task_subject.clone(),
                task_description: task_description.clone(),
                teammate_name: teammate_name.clone(),
                team_name: team_name.clone(),
            };
            Some(("TaskCompleted", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::TaskCreated {
            task_id,
            task_type,
            description,
            teammate_name,
            team_name,
        } => {
            // `executeTaskCreatedHooks` (`utils/hooks.ts:3756-3764`): the wire
            // payload carries `task_subject` (required) + optional
            // `task_description` / `teammate_name` / `team_name`. The Rust
            // `TaskCreated` variant sources the subject from the task's
            // `task_type` taxonomy bucket and the description from
            // `description`; `teammate_name` / `team_name` ride from the
            // creating teammate's identity (TS `getAgentName()` / `getTeamName()`)
            // when bound, else `None`.
            let payload = TaskCreatedPayload {
                hook_event_name: HookEventNameTaskCreated,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                task_id: task_id.clone(),
                task_subject: task_type.clone(),
                task_description: Some(description.clone()),
                teammate_name: teammate_name.clone(),
                team_name: team_name.clone(),
            };
            Some(("TaskCreated", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::UserPromptSubmit { prompt } => {
            let payload = UserPromptSubmitPayload {
                hook_event_name: HookEventNameUserPromptSubmit,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                prompt: prompt.clone(),
                session_title: b.session_title,
            };
            Some(("UserPromptSubmit", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::SessionStart { source, .. } => {
            let payload = SessionStartPayload {
                hook_event_name: HookEventNameSessionStart,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                source: source.clone(),
                agent_type: b.agent_type,
                effort: b.effort,
                model: None,
                session_title: b.session_title,
            };
            Some(("SessionStart", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::StopFailure { error } => {
            let payload = StopFailurePayload {
                hook_event_name: HookEventNameStopFailure,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                error: error.clone(),
                error_details: None,
                last_assistant_message: ctx.last_assistant_message.clone(),
            };
            Some(("StopFailure", serde_json::to_string(&payload).ok()?))
        }
        // B6 — additional events whose `HookEvent` variant already exists.
        // Where the variant carries fewer fields than the wire schema, the
        // missing fields default exactly as the B1 arms above (`""` /
        // `Value::Null` / `None`) until richer context is threaded through
        // `HookEvent` / `HookContext`. (`Setup`/`PostCompact` now carry the
        // real `trigger` from the firing site.)
        // `PostToolUseFailure` now carries the dispatched `tool_input` (the
        // same `effective_input` the `PostToolUse` arm threads), matching the
        // claude-code `PostToolUseFailure` input schema.
        HookEvent::PostToolUseFailure {
            tool_name,
            tool_input,
            error,
            tool_use_id,
            duration_ms,
        } => {
            let payload = PostToolUseFailurePayload {
                hook_event_name: HookEventNamePostToolUseFailure,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                tool_use_id: tool_use_id.to_string(),
                error: error.clone(),
                is_interrupt: None,
                duration_ms: *duration_ms,
            };
            Some(("PostToolUseFailure", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::SessionEnd { reason, .. } => {
            let payload = SessionEndPayload {
                hook_event_name: HookEventNameSessionEnd,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                reason: reason.clone(),
            };
            Some(("SessionEnd", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::PreCompact {
            reason,
            custom_instructions,
        } => {
            let payload = PreCompactPayload {
                hook_event_name: HookEventNamePreCompact,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                // `HookEvent::PreCompact.reason` is the `manual` / `auto`
                // trigger in the wire schema.
                trigger: reason.clone(),
                custom_instructions: custom_instructions.clone(),
            };
            Some(("PreCompact", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::PostCompact {
            summary, trigger, ..
        } => {
            let payload = PostCompactPayload {
                hook_event_name: HookEventNamePostCompact,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                trigger: trigger.clone(),
                compact_summary: summary.clone(),
            };
            Some(("PostCompact", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::PreModelSwitch {
            from_model,
            to_model,
            requested_model,
            source,
            context_tokens,
            prompt_cache_warm,
            cache_ttl,
            estimated_cache_write_usd,
            pricing,
        } => {
            let payload = PreModelSwitchPayload {
                hook_event_name: HookEventNamePreModelSwitch,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                from_model: from_model.clone(),
                to_model: to_model.clone(),
                requested_model: requested_model.clone(),
                source: source.clone(),
                context_tokens: *context_tokens,
                prompt_cache_warm: *prompt_cache_warm,
                cache_ttl: cache_ttl.clone(),
                estimated_cache_write_usd: *estimated_cache_write_usd,
                pricing: pricing.clone(),
            };
            Some(("PreModelSwitch", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::PostModelSwitch {
            from_model,
            to_model,
            requested_model,
            source,
            context_tokens,
            prompt_cache_warm,
            cache_ttl,
            estimated_cache_write_usd,
            pricing,
        } => {
            let payload = PostModelSwitchPayload {
                hook_event_name: HookEventNamePostModelSwitch,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                from_model: from_model.clone(),
                to_model: to_model.clone(),
                requested_model: requested_model.clone(),
                source: source.clone(),
                context_tokens: *context_tokens,
                prompt_cache_warm: *prompt_cache_warm,
                cache_ttl: cache_ttl.clone(),
                estimated_cache_write_usd: *estimated_cache_write_usd,
                pricing: pricing.clone(),
            };
            Some(("PostModelSwitch", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::Notification { message, kind } => {
            let payload = NotificationPayload {
                hook_event_name: HookEventNameNotification,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                message: message.clone(),
                title: None,
                notification_type: kind.clone(),
            };
            Some(("Notification", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::PermissionRequest {
            tool_name,
            tool_input,
            ..
        } => {
            let payload = PermissionRequestPayload {
                hook_event_name: HookEventNamePermissionRequest,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                permission_suggestions: None,
            };
            Some(("PermissionRequest", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::PermissionDenied {
            tool_name,
            tool_input,
            tool_use_id,
            reason,
        } => {
            let payload = PermissionDeniedPayload {
                hook_event_name: HookEventNamePermissionDenied,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                tool_use_id: tool_use_id.to_string(),
                reason: reason.clone(),
            };
            Some(("PermissionDenied", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::Setup { trigger } => {
            let payload = SetupPayload {
                hook_event_name: HookEventNameSetup,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                trigger: trigger.clone(),
            };
            Some(("Setup", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::AgentSpawn {
            agent_type,
            model,
            cwd,
            background,
            parent_agent_id,
        } => {
            let payload = crate::hook_payload::AgentSpawnPayload {
                hook_event_name: crate::hook_payload::HookEventNameAgentSpawn,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: cwd.clone().unwrap_or(b.cwd),
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_type: agent_type.clone(),
                model: model.clone(),
                background: *background,
                parent_agent_id: parent_agent_id.as_ref().map(ToString::to_string),
            };
            Some(("AgentSpawn", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::SubagentStart {
            agent_id,
            agent_type,
            ..
        } => {
            let payload = SubagentStartPayload {
                hook_event_name: HookEventNameSubagentStart,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: agent_id.to_string(),
                agent_type: agent_type.clone(),
                effort: b.effort,
            };
            Some(("SubagentStart", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::CwdChanged { old, new } => {
            let payload = CwdChangedPayload {
                hook_event_name: HookEventNameCwdChanged,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                old_cwd: old.to_string_lossy().into_owned(),
                new_cwd: new.to_string_lossy().into_owned(),
            };
            Some(("CwdChanged", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::DirectoryAdded { directory, source } => {
            let payload = DirectoryAddedPayload {
                hook_event_name: HookEventNameDirectoryAdded,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                directory: directory.clone(),
                source: source.clone(),
            };
            Some(("DirectoryAdded", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::FileChanged { path, kind } => {
            let payload = FileChangedPayload {
                hook_event_name: HookEventNameFileChanged,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                file_path: path.to_string_lossy().into_owned(),
                event: kind.clone(),
            };
            Some(("FileChanged", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::WorktreeRemove { path } => {
            let payload = WorktreeRemovePayload {
                hook_event_name: HookEventNameWorktreeRemove,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                worktree_path: path.to_string_lossy().into_owned(),
            };
            Some(("WorktreeRemove", serde_json::to_string(&payload).ok()?))
        }
        // Deferred-completion batch — the final four events. Their `HookEvent`
        // variant now carries the field(s) needed to source each *required*
        // wire value, so they serialize faithfully.
        //
        // claude-code parity note on `permission_mode`: `ConfigChange`,
        // `InstructionsLoaded`, and `WorktreeCreate` build their base shape with
        // `createBaseHookInput(undefined)` (`utils/hooks.ts:4220` / `4354` /
        // `4932`), so they emit NO `permission_mode` regardless of the engine's
        // current mode — we pass `None` rather than `b.permission_mode`.
        // `Elicitation` alone uses `createBaseHookInput(permissionMode)`
        // (`utils/hooks.ts:4492`), so it threads `b.permission_mode`.
        HookEvent::ConfigChange { source, file_path } => {
            let payload = ConfigChangePayload {
                hook_event_name: HookEventNameConfigChange,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: None,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                source: *source,
                file_path: file_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            };
            Some(("ConfigChange", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::InstructionsLoaded {
            file_path,
            memory_type,
            load_reason,
            globs,
            trigger_file_path,
            parent_file_path,
        } => {
            let payload = InstructionsLoadedPayload {
                hook_event_name: HookEventNameInstructionsLoaded,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: None,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                file_path: file_path.to_string_lossy().into_owned(),
                memory_type: *memory_type,
                load_reason: *load_reason,
                globs: globs.clone(),
                trigger_file_path: trigger_file_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
                parent_file_path: parent_file_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned()),
            };
            Some(("InstructionsLoaded", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::Elicitation {
            server_name,
            message,
            mode,
            url,
            elicitation_id,
            requested_schema,
        } => {
            let payload = ElicitationPayload {
                hook_event_name: HookEventNameElicitation,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                mcp_server_name: server_name.clone(),
                message: message.clone(),
                mode: *mode,
                url: url.clone(),
                elicitation_id: elicitation_id.clone(),
                requested_schema: requested_schema.clone(),
            };
            Some(("Elicitation", serde_json::to_string(&payload).ok()?))
        }
        HookEvent::WorktreeCreate { name, .. } => {
            let payload = WorktreeCreatePayload {
                hook_event_name: HookEventNameWorktreeCreate,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: None,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                name: name.clone(),
            };
            Some(("WorktreeCreate", serde_json::to_string(&payload).ok()?))
        }
        // `executeTeammateIdleHooks` (`utils/hooks.ts:3716-3720`): the wire
        // payload carries `teammate_name` + `team_name` (BOTH required strings).
        // Unlike `ConfigChange` / `InstructionsLoaded` / `WorktreeCreate`, this
        // fires through `createBaseHookInput(permissionMode)` (the `permissionMode`
        // threaded from `stopHooks.ts`), so it emits `permission_mode` — same as
        // `Elicitation`. claude-code sources `team_name` from `getTeamName() ?? ''`,
        // so a `""` here is faithful when the firing scope has no team identity.
        HookEvent::TeammateIdle {
            teammate_name,
            team_name,
        } => {
            let payload = TeammateIdlePayload {
                session_id: ctx.session_id.as_uuid().to_string(),
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                // No scratchpad allocator is wired to this hook scope. Omit the
                // optional field rather than fabricate a directory.
                scratchpad_dir: None,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_type: b.agent_type,
                hook_event_name: HookEventNameTeammateIdle,
                teammate_name: teammate_name.clone(),
                team_name: team_name.clone(),
            };
            Some(("TeammateIdle", serde_json::to_string(&payload).ok()?))
        }
        // #39 PostToolBatch (claude-code `G4t`, BIN off 205710327): the full
        // batch of resolved tool calls. Fires through `vd(r,void 0,n)` so it
        // carries the base shape (permission_mode/agent_type from ctx).
        HookEvent::PostToolBatch { tool_calls } => {
            let payload = PostToolBatchPayload {
                hook_event_name: HookEventNamePostToolBatch,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                tool_calls: tool_calls.clone(),
            };
            Some(("PostToolBatch", serde_json::to_string(&payload).ok()?))
        }
        // #39 UserPromptExpansion (claude-code `b$t`, BIN off 201270310).
        HookEvent::UserPromptExpansion {
            expansion_type,
            command_name,
            command_args,
            command_source,
            prompt,
        } => {
            let payload = UserPromptExpansionPayload {
                hook_event_name: HookEventNameUserPromptExpansion,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                expansion_type: *expansion_type,
                command_name: command_name.clone(),
                command_args: command_args.clone(),
                command_source: command_source.clone(),
                prompt: prompt.clone(),
            };
            Some(("UserPromptExpansion", serde_json::to_string(&payload).ok()?))
        }
        // #39 MessageDisplay (claude-code `aAt`, BIN off 205705090). Fires
        // through `vd(void 0)` (no permission mode), with `forceSyncExecution`
        // + `suppressPerInvocationTelemetry` — those two are firing-site flags,
        // not wire fields, so they do not appear in the payload.
        HookEvent::MessageDisplay {
            turn_id,
            message_id,
            index,
            is_final,
            delta,
        } => {
            let payload = MessageDisplayPayload {
                hook_event_name: HookEventNameMessageDisplay,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                // `vd(void 0)` => no permission mode threaded.
                permission_mode: None,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                turn_id: turn_id.clone(),
                message_id: message_id.clone(),
                index: *index,
                is_final: *is_final,
                delta: delta.clone(),
            };
            Some(("MessageDisplay", serde_json::to_string(&payload).ok()?))
        }
        // `executeElicitationResultHooks` (BIN off 67628403). Wire schema
        // (binary-confirmed at BIN off ~201751493):
        // `{hook_event_name:"ElicitationResult", mcp_server_name:string,
        //   elicitation_id?:string, mode?:enum(["form","url"]),
        //   action:enum(["accept","decline","cancel"]),
        //   content?:record(string,unknown)}`.
        // The `action` and `content` fields are extracted from the complete
        // result JSON; id and mode retain the originating elicitation identity.
        // Uses `createBaseHookInput(permissionMode)` (same as `Elicitation`).
        HookEvent::ElicitationResult {
            server_name,
            elicitation_id,
            mode,
            result,
        } => {
            // Extract `action` from the result JSON. The binary schema requires
            // it; fall back to `"cancel"` (the safe default) when absent, so the
            // hook process still receives a valid payload even if the engine
            // didn't capture the action.
            let action = result
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("cancel")
                .to_string();
            // `content` is a record (optional): pass it through as-is when present.
            let content = result.get("content").cloned();
            let payload = ElicitationResultPayload {
                hook_event_name: HookEventNameElicitationResult,
                session_id: b.session_id,
                transcript_path: b.transcript_path,
                cwd: b.cwd,
                prompt_id: b.prompt_id,
                permission_mode: b.permission_mode,
                agent_id: b.agent_id,
                agent_type: b.agent_type,
                effort: b.effort,
                mcp_server_name: server_name.clone(),
                elicitation_id: elicitation_id.clone(),
                mode: *mode,
                action,
                content,
            };
            Some(("ElicitationResult", serde_json::to_string(&payload).ok()?))
        }
        _ => None,
    }
}

/// Build the `(HookResult, timed_out)` pair for a non-timeout process error.
fn process_error_outcome(hook: &HookDefinition, e: &ProcessError) -> (HookResult, bool) {
    (
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: format!("Hook {} failed: process error: {e}", hook.id),
            exit_code: None,
            response: None,
        },
        false,
    )
}

/// Map a [`ProcessRunner::run`] result onto a [`HookResult`] per the
/// claude-code command-hook contract (`claude-code/src/utils/hooks.ts`).
///
/// Returns `(result, timed_out)` where the bool tells the caller whether to
/// emit `HOOK_TIMEOUT` telemetry.
///
/// Decision layering (mirrors `hooks.ts:2499-2697`):
/// 1. If stdout (trimmed) starts with `{`, parse and validate it via
///    [`parse_response`]. A parsed `HookResponse.decision` drives blocking
///    through `merge`; a parse/validation failure at a non-2 exit code is a
///    non-blocking hook error.
/// 2. A JSON parse/validation failure at exit 2 falls through to the same
///    blocking stderr fallback as plain text (the process explicitly chose the
///    hook's blocking exit status).
/// 3. Otherwise apply the exit-code fallback:
///    - `0` ⇒ success, no decision.
///    - `2` ⇒ **block**, with stderr as the reason (`hooks.ts:2648-2666`).
///    - any other non-zero ⇒ non-blocking error (`Error`, no `Block` decision).
fn map_command_output(
    hook: &HookDefinition,
    run: Result<mobile_linux_api::ProcessOutput, ProcessError>,
    expected_event: &'static str,
) -> (HookResult, bool) {
    match run {
        Err(ProcessError::Timeout) => (
            HookResult {
                outcome: HookOutcome::Timeout,
                stdout: String::new(),
                stderr: format!("Hook {} failed: command timed out", hook.id),
                exit_code: None,
                response: None,
            },
            true,
        ),
        Err(
            e @ (ProcessError::Io(_)
            | ProcessError::Unsupported
            | ProcessError::PolicyUnsupported(_)
            | ProcessError::MalformedSandboxPlan(_)
            | ProcessError::SandboxEnforcementFailed(_)),
        ) => process_error_outcome(hook, &e),
        Ok(o) if o.timed_out => (
            HookResult {
                outcome: HookOutcome::Timeout,
                stdout: o.stdout,
                stderr: o.stderr,
                exit_code: Some(o.exit_code),
                response: None,
            },
            true,
        ),
        Ok(o) => map_text_hook_output(hook, o.stdout, o.stderr, o.exit_code, expected_event),
    }
}

fn map_text_hook_output(
    hook: &HookDefinition,
    stdout: String,
    stderr: String,
    exit_code: i32,
    expected_event: &'static str,
) -> (HookResult, bool) {
    if stdout.trim_start().starts_with('{') {
        match parse_response(&stdout, expected_event) {
            Ok(parsed) => {
                if expected_event == "PostModelSwitch" && exit_code != 0 {
                    return (
                        HookResult {
                            outcome: HookOutcome::Error,
                            stdout,
                            stderr,
                            exit_code: Some(exit_code),
                            response: None,
                        },
                        false,
                    );
                }
                return (
                    HookResult {
                        outcome: if exit_code == 0 {
                            HookOutcome::Success
                        } else {
                            HookOutcome::Error
                        },
                        stdout,
                        stderr,
                        exit_code: Some(exit_code),
                        response: Some(parsed),
                    },
                    false,
                );
            }
            Err(error) if exit_code != 2 => {
                return (
                    HookResult {
                        outcome: HookOutcome::Error,
                        stdout,
                        stderr: error.to_string(),
                        exit_code: Some(exit_code),
                        response: None,
                    },
                    false,
                );
            }
            Err(_) => {}
        }
    }
    if expected_event == "PermissionRequest" && exit_code == 2 {
        return (
            HookResult {
                outcome: HookOutcome::Error,
                stdout,
                stderr,
                exit_code: Some(2),
                response: None,
            },
            false,
        );
    }
    if expected_event == "PostModelSwitch" && exit_code != 0 {
        return (
            HookResult {
                outcome: HookOutcome::Error,
                stdout,
                stderr,
                exit_code: Some(exit_code),
                response: None,
            },
            false,
        );
    }
    match exit_code {
        0 => (
            HookResult {
                outcome: HookOutcome::Success,
                stdout,
                stderr,
                exit_code: Some(0),
                response: None,
            },
            false,
        ),
        2 => {
            let display = match &hook.executor {
                HookExecutor::Command { command, args, .. } if !args.is_empty() => {
                    std::iter::once(command.as_str())
                        .chain(args.iter().map(String::as_str))
                        .collect::<Vec<_>>()
                        .join(" ")
                }
                HookExecutor::Command { command, .. } => command.clone(),
                HookExecutor::McpTool { server, tool, .. } => format!("{server}/{tool}"),
                _ => hook.name.clone(),
            };
            let body = if stderr.is_empty() {
                "No stderr output"
            } else {
                stderr.as_str()
            };
            let reason = format!("[{display}]: {body}");
            (
                HookResult {
                    outcome: HookOutcome::Error,
                    stdout,
                    stderr,
                    exit_code: Some(2),
                    response: Some(HookResponse {
                        decision: Some(HookDecision::Block),
                        reason: Some(reason),
                        block_command: Some(display),
                        ..HookResponse::default()
                    }),
                },
                false,
            )
        }
        other => (
            HookResult {
                outcome: HookOutcome::Error,
                stdout,
                stderr,
                exit_code: Some(other),
                response: None,
            },
            false,
        ),
    }
}

fn join_mcp_text_content(content: &[String]) -> String {
    content.join("\n")
}

fn expand_mcp_hook_input_map(
    input: &HashMap<String, Value>,
    payload: &Value,
) -> HashMap<String, Value> {
    input
        .iter()
        .map(|(key, value)| (key.clone(), expand_mcp_hook_value(value, payload)))
        .collect()
}

fn expand_mcp_hook_value(value: &Value, payload: &Value) -> Value {
    match value {
        Value::String(text) => expand_mcp_hook_string(text, payload),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| expand_mcp_hook_value(item, payload))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), expand_mcp_hook_value(item, payload)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn expand_mcp_hook_string(text: &str, payload: &Value) -> Value {
    if text.starts_with("${") && text.ends_with('}') && text.matches("${").count() == 1 {
        let path = &text[2..text.len() - 1];
        if let Some(value) = lookup_mcp_hook_path(payload, path) {
            return value.clone();
        }
    }
    let mut out = String::new();
    let mut cursor = 0;
    while let Some(start_rel) = text[cursor..].find("${") {
        let start = cursor + start_rel;
        out.push_str(&text[cursor..start]);
        let path_start = start + 2;
        let Some(close_rel) = text[path_start..].find('}') else {
            out.push_str(&text[start..]);
            return Value::String(out);
        };
        let path_end = path_start + close_rel;
        let replacement = lookup_mcp_hook_path(payload, &text[path_start..path_end])
            .map(render_mcp_hook_value)
            .unwrap_or_default();
        out.push_str(&replacement);
        cursor = path_end + 1;
    }
    out.push_str(&text[cursor..]);
    Value::String(out)
}

fn lookup_mcp_hook_path<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = payload;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        match current {
            Value::Object(map) => current = map.get(segment)?,
            Value::Array(items) => current = items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        }
    }
    Some(current)
}

fn render_mcp_hook_value(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// Build the async-registry finalizer that persists the winning terminal
/// outcome, including a timeout synthesized outside the hook work future.
fn attachment_completion(
    sink: Option<Arc<dyn HookAttachmentSink>>,
    hook: HookDefinition,
    identity: HookAttachmentIdentity,
    run_started: std::time::Instant,
    observer: Option<Arc<dyn OutputStream>>,
    progress_id: Option<String>,
) -> Option<HookCompletion> {
    if sink.is_none() && observer.is_none() {
        return None;
    }
    Some(Box::new(move |result| {
        Box::pin(async move {
            let transferred_again = is_runtime_async_backgrounded(&result);
            #[allow(clippy::cast_possible_truncation)]
            let run_ms = run_started.elapsed().as_millis() as u64;
            if let Some(sink) = sink {
                if let Some(value) =
                    build_run_attachment_persisting(Some(&sink), &hook, &identity, &result, run_ms)
                        .await
                {
                    sink.record(value).await;
                }
            }
            // A configured-async command can itself print the runtime async
            // marker. In that case the nested registry completion owns this
            // same progress id; closing it here would recreate the early-finish
            // race one layer higher.
            if !transferred_again {
                if let (Some(observer), Some(progress_id)) = (observer, progress_id) {
                    observer.emit_hook_progress_finished(&progress_id).await;
                }
            }
        })
    }))
}

fn is_runtime_async_backgrounded(result: &HookResult) -> bool {
    result
        .response
        .as_ref()
        .is_some_and(|response| response.async_backgrounded)
}

/// Mint the identity fields shared by every attachment produced for one
/// `execute*` dispatch.
///
/// claude threads ONE `toolUseID` through the whole runner, so every hook
/// matched by a single dispatch shares it: the provider tool-use id for tool
/// events, otherwise a freshly minted uuid.
fn attachment_identity(event: &HookEvent) -> HookAttachmentIdentity {
    HookAttachmentIdentity {
        hook_name: attachment::hook_name_for_event(event),
        hook_event: format!("{:?}", event.event_type()),
        tool_use_id: attachment::tool_use_id_for_event(event)
            .unwrap_or_else(|| protocol::HookId::new().as_uuid().to_string()),
    }
}

/// Build the ONE transcript `attachment` payload for a completed hook run, or
/// `None` for a run that BLOCKED or was moved to the runtime-marker async path.
///
/// Outcome → payload mapping, per the 2.1.220 command-hook runner
/// (BIN off 237798900–237806040):
///
/// | run outcome | claude arm | payload |
/// |---|---|---|
/// | `Success` | `Ce.status===0` / `Tfn` | `hook_success` |
/// | `Error` (non-blocking) | `Ce.status` non-zero, non-2 | `hook_non_blocking_error` |
/// | `Error` + `Block` decision | `Ce.status===2` | *(none — `hook_blocking_error`)* |
/// | `Timeout` / `Cancelled` | `Ce.aborted` | `hook_cancelled` |
///
/// `content` on `hook_success` is `""` whenever the hook returned parsable
/// JSON — claude routes that through `Tfn`, which hardcodes `content:""`
/// (BIN off 237778629); all 25 901 mined `hook_success` records have it empty.
/// Plain-text output instead carries `jKe(stdout.trim())`.
fn build_run_attachment(
    hook: &HookDefinition,
    id: &HookAttachmentIdentity,
    result: &HookResult,
    elapsed_ms: u64,
) -> Option<serde_json::Value> {
    // `{"async":true}` returns this empty success only to keep the originating
    // turn non-blocking. It is not a completed run and must never be persisted;
    // the Dispatcher records the eventual result once the detached process
    // actually finishes.
    if is_runtime_async_backgrounded(result)
        || (matches!(&hook.executor, HookExecutor::Command { .. })
            && matches!(result.outcome, HookOutcome::Success)
            && result.exit_code.is_none()
            && result.stdout.is_empty()
            && result.stderr.is_empty()
            && result.response.is_none())
    {
        return None;
    }
    // A blocking run yields `{blockingError, outcome:"blocking"}` with a
    // `hook_blocking_error` attachment, never a run-outcome one.
    if result
        .response
        .as_ref()
        .and_then(|r| r.decision)
        .is_some_and(|d| matches!(d, HookDecision::Block))
    {
        return None;
    }
    // `command` = `qq(hook)` — statusMessage, else the per-arm rendering.
    let command = attachment::attachment_command(hook);
    match result.outcome {
        HookOutcome::Success => {
            let content = if result.response.is_some() {
                String::new()
            } else {
                attachment::inline_hook_output(result.stdout.trim())
            };
            Some(attachment::success_attachment(
                id,
                &content,
                &result.stdout,
                &result.stderr,
                // Non-process arms carry no status; claude's mcp_tool success
                // arm likewise reports 0.
                result.exit_code.unwrap_or(0),
                &command,
                elapsed_ms,
            ))
        }
        HookOutcome::Error => Some(attachment::non_blocking_error_attachment(
            id,
            &result.stderr,
            &result.stdout,
            // claude's spawn-failure arm reports `exitCode:1` when the child
            // never produced a status (BIN off 237806040).
            result.exit_code.unwrap_or(1),
            Some(&command),
            Some(elapsed_ms),
        )),
        HookOutcome::Timeout | HookOutcome::Cancelled => {
            Some(attachment::cancelled_attachment(
                id,
                Some(&command),
                Some(elapsed_ms),
                Some(CancellationTimeout {
                    // `timedOut: !outerSignal?.aborted` — true when the hook's
                    // own deadline fired, false when the caller aborted it.
                    timed_out: matches!(result.outcome, HookOutcome::Timeout),
                    timeout_ms: attachment_timeout_ms(hook),
                }),
            ))
        }
    }
}

async fn persist_large_hook_output(
    sink: Option<&Arc<dyn HookAttachmentSink>>,
    text: &str,
) -> Option<String> {
    if text.chars().count() <= attachment::HOOK_OUTPUT_INLINE_LIMIT {
        return None;
    }
    match sink {
        Some(sink) => sink.persist_large_output(text).await,
        None => None,
    }
}

async fn build_run_attachment_persisting(
    sink: Option<&Arc<dyn HookAttachmentSink>>,
    hook: &HookDefinition,
    id: &HookAttachmentIdentity,
    result: &HookResult,
    elapsed_ms: u64,
) -> Option<serde_json::Value> {
    let mut value = build_run_attachment(hook, id, result, elapsed_ms)?;
    if matches!(result.outcome, HookOutcome::Success) && result.response.is_none() {
        if let Some(reference) = persist_large_hook_output(sink, result.stdout.trim()).await {
            value["content"] = serde_json::Value::String(reference);
        }
    }
    Some(value)
}

/// The deadline in force for a hook — `re = q.timeout ? q.timeout*1000 : i`
/// (BIN off 237797884), where `i` is the runner's per-arm default.
fn attachment_timeout_ms(hook: &HookDefinition) -> u64 {
    if !hook.blocking {
        #[allow(clippy::cast_possible_truncation)]
        return hook
            .async_timeout
            .unwrap_or_else(|| Duration::from_millis(DEFAULT_ASYNC_HOOK_TIMEOUT_MS))
            .as_millis() as u64;
    }
    if let Some(t) = hook.timeout {
        // hook timeouts are seconds-scale — u128 ms cannot exceed u64::MAX
        #[allow(clippy::cast_possible_truncation)]
        return t.as_millis() as u64;
    }
    match &hook.executor {
        HookExecutor::Http { .. } => HOOK_HTTP_TIMEOUT_MS,
        HookExecutor::Agent { .. } => HOOK_AGENT_TIMEOUT_MS,
        HookExecutor::Prompt { .. } => HOOK_PROMPT_TIMEOUT_MS,
        // A function hook enforces its own budget inside the sandbox
        // (`function_hook::Sandbox::budget`), so the outer hook timeout is a
        // backstop rather than the live cap.
        HookExecutor::Function { .. } => HOOK_FUNCTION_TIMEOUT_MS,
        // `mcp_tool` carries only a per-hook `timeout` in the oracle schema
        // ("Timeout in seconds for this specific tool call") with no arm-specific
        // default, so an entry that omits it falls back to the generic hook
        // default `q_ = 600000` — the same constant the Command/Builtin arms use.
        HookExecutor::Command { .. }
        | HookExecutor::Builtin { .. }
        | HookExecutor::McpTool { .. } => HOOK_COMMAND_TIMEOUT_MS,
    }
}

/// Emit `HOOK_TIMEOUT` telemetry for a Command hook that exceeded its
/// timeout (mirrors [`emit_http_signal`] / [`emit_agent_signal`]).
fn emit_command_timeout(hook: &HookDefinition, timeout: Duration) {
    // hook timeout bounded to seconds — u128 ms cannot exceed u64::MAX
    #[allow(clippy::cast_possible_truncation)]
    let timeout_ms = timeout.as_millis() as u64;
    tracing::info!(
        event = telemetry::tengu::orchestrator::HOOK_TIMEOUT,
        hook_id = %hook.id,
        hook_kind = "command",
        timeout_ms = timeout_ms,
    );
}

fn emit_mcp_timeout(hook: &HookDefinition, timeout: Duration) {
    #[allow(clippy::cast_possible_truncation)]
    let timeout_ms = timeout.as_millis() as u64;
    tracing::info!(
        event = telemetry::tengu::orchestrator::HOOK_TIMEOUT,
        hook_id = %hook.id,
        hook_kind = "mcp_tool",
        timeout_ms = timeout_ms,
    );
}

/// Emit `HOOK_TIMEOUT` telemetry when a `SessionEnd` hook is cut off by the
/// *batch* shutdown deadline (claude-code `Wqt`), distinct from a per-hook
/// timeout. `batch_timeout_ms` is the whole-batch budget the hook overran.
fn emit_session_end_batch_timeout(hook: &HookDefinition, batch_timeout_ms: u64) {
    tracing::info!(
        event = telemetry::tengu::orchestrator::HOOK_TIMEOUT,
        hook_id = %hook.id,
        hook_kind = "session_end_batch",
        timeout_ms = batch_timeout_ms,
    );
}

/// Emit arm-level telemetry for an HTTP signal (SSRF / timeout). Other
/// telemetry (`HOOK_PRE_*` / `HOOK_POST_*`) is fired by the orchestrator's
/// `dispatch_tool_with_hooks` (M5-06 Task 14).
fn emit_http_signal(hook: &HookDefinition, signal: &HttpExecutionSignal, timeout: Duration) {
    match signal {
        HttpExecutionSignal::SsrfBlocked(reason) => {
            tracing::info!(
                event = telemetry::tengu::orchestrator::HOOK_HTTP_SKIPPED_SSRF,
                hook_id = %hook.id,
                reason = %reason,
            );
        }
        HttpExecutionSignal::TimedOut => {
            // hook timeout bounded to seconds — u128 ms cannot exceed u64::MAX
            #[allow(clippy::cast_possible_truncation)]
            let timeout_ms = timeout.as_millis() as u64;
            tracing::info!(
                event = telemetry::tengu::orchestrator::HOOK_TIMEOUT,
                hook_id = %hook.id,
                hook_kind = "http",
                timeout_ms = timeout_ms,
            );
        }
        // H-BIN-12: CC's `allowedHttpHookUrls` block emits NO telemetry event —
        // only the byte-exact warn line (fired in `HttpExecutor::execute`).
        HttpExecutionSignal::UrlBlocked => {}
        HttpExecutionSignal::Ok => {}
    }
}

fn emit_agent_signal(hook: &HookDefinition, signal: &AgentExecutionSignal, timeout: Duration) {
    if matches!(signal, AgentExecutionSignal::TimedOut) {
        // hook timeout bounded to seconds — u128 ms cannot exceed u64::MAX
        #[allow(clippy::cast_possible_truncation)]
        let timeout_ms = timeout.as_millis() as u64;
        tracing::info!(
            event = telemetry::tengu::orchestrator::HOOK_TIMEOUT,
            hook_id = %hook.id,
            hook_kind = "agent",
            timeout_ms = timeout_ms,
        );
    }
}

/// Emit arm-level telemetry for a Prompt signal (timeout). Mirrors
/// [`emit_agent_signal`]; the success / not-met / parse-error / not-wired
/// signals carry no dedicated telemetry event (parity with the Agent arm,
/// which only emits on timeout).
fn emit_prompt_signal(hook: &HookDefinition, signal: &PromptExecutionSignal, timeout: Duration) {
    if matches!(signal, PromptExecutionSignal::TimedOut) {
        // hook timeout bounded to seconds — u128 ms cannot exceed u64::MAX
        #[allow(clippy::cast_possible_truncation)]
        let timeout_ms = timeout.as_millis() as u64;
        tracing::info!(
            event = telemetry::tengu::orchestrator::HOOK_TIMEOUT,
            hook_id = %hook.id,
            hook_kind = "prompt",
            timeout_ms = timeout_ms,
        );
    }
}

#[cfg(test)]
#[path = "executor_test.rs"]
mod executor_test;

/// PARITY 2.1.263 `YYe()` — `process.env.CLAUDE_CODE_EVAL_CONFINED === true`.
///
/// A confined eval-harness run takes its permission grants ONLY from the
/// command line: hook allows are dropped ([`run_hooks`]'s `H_n` fold) and the
/// rule loader drops every `allow`-behavior rule (`OG(e)` — NOT yet ported; see
/// `docs/permission-byte-alignment-2.1.263-2026-09-07.md`).
///
/// The binary compares against the literal `true`, so `1`/`yes` do NOT arm it;
/// the port keeps that exact spelling rather than the usual truthy allowlist.
#[must_use]
pub fn eval_confined_session() -> bool {
    platform_api::env::is_eval_confined_session()
}

// Hook-run transcript attachments (`hook_success` / `hook_non_blocking_error` /
// `hook_cancelled`). Kept in this file rather than the `executor_test.rs`
// sibling per the repo's concurrent-edit convention.
#[cfg(test)]
mod attachment_wiring_tests {
    use super::*;
    use crate::attachment::HookAttachmentSink;
    use crate::definition::{HookDefinition, HookSource};
    use crate::events::HookEventType;
    use crate::registry::HookRegistry;
    use mobile_linux_api::{ProcessOutput, SandboxBackend};
    use platform_api::{
        ProcessHandle, RuntimeError, SandboxCapability, SandboxPolicy, SandboxedCommand,
        SandboxedTag,
    };
    use protocol::{HookId, ToolUseId};
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        seen: Mutex<Vec<Value>>,
        large_outputs: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl HookAttachmentSink for RecordingSink {
        async fn record(&self, attachment: Value) {
            self.seen.lock().unwrap().push(attachment);
        }

        async fn persist_large_output(&self, text: &str) -> Option<String> {
            self.large_outputs.lock().unwrap().push(text.to_string());
            Some("(Full output saved to: /session/tool-results/hook.txt)".into())
        }
    }

    struct FixedRunner(Mutex<Option<Result<ProcessOutput, ProcessError>>>);

    #[async_trait]
    impl ProcessRunner for FixedRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            self.0
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Err(ProcessError::Io("exhausted".into())))
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    struct StubSandbox;
    #[async_trait]
    impl Sandbox for StubSandbox {
        fn is_available(&self) -> bool {
            true
        }
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::None
        }
        fn prepare(
            &self,
            cmd: ProcessCommand,
            _policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, platform_api::SandboxError> {
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: "test".into(),
                },
            ))
        }
        fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
            SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: reason.into(),
                },
            )
        }
        async fn probe_capability(&self) -> SandboxCapability {
            SandboxCapability {
                available: true,
                reason: None,
                features: platform_api::SandboxFeatures::default(),
            }
        }
    }

    struct UnusedRuntime;
    #[async_trait]
    impl RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<platform_api::BackgroundTaskHandle, RuntimeError> {
            Err(RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: Duration) {}
        async fn cancel(
            &self,
            _handle: &platform_api::BackgroundTaskHandle,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    struct UnusedHttp;
    #[async_trait]
    impl HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
    }

    fn post_hook() -> HookDefinition {
        HookDefinition {
            id: HookId::new(),
            name: "fmt".into(),
            events: vec![HookEventType::PostToolUse],
            if_condition: None,
            executor: HookExecutor::Command {
                command: "./hooks/fmt.sh".into(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
                shell: None,
            },
            source: HookSource::Settings(protocol::SettingsScope::User),
            blocking: true,
            timeout: Some(Duration::from_secs(90)),
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        }
    }

    fn post_event() -> HookEvent {
        HookEvent::PostToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
            tool_output: serde_json::json!({"stdout": "x"}),
            tool_use_id: ToolUseId::from("toolu_01ApkBwAZMCAza47B5nAWiGS".to_string()),
            duration_ms: None,
        }
    }

    fn exec_with(
        run: Result<ProcessOutput, ProcessError>,
        sink: Arc<RecordingSink>,
    ) -> HookExecutorImpl {
        let mut registry = HookRegistry::new();
        registry.register(post_hook());
        HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_process_runner(
            Arc::new(FixedRunner(Mutex::new(Some(run)))),
            Arc::new(StubSandbox),
        )
        .with_attachment_sink(sink)
    }

    fn out(stdout: &str, stderr: &str, exit_code: i32) -> ProcessOutput {
        ProcessOutput {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            timed_out: false,
        }
    }

    /// One `hook_success` attachment per successful hook run, published to the
    /// sink AND carried on the aggregate.
    #[tokio::test]
    async fn successful_run_publishes_one_hook_success_attachment() {
        let sink = Arc::new(RecordingSink::default());
        let exec = exec_with(Ok(out("formatted\n", "", 0)), sink.clone());

        let agg = exec.execute(post_event(), HookContext::default()).await;

        assert_eq!(agg.hook_attachments.len(), 1, "exactly one per hook run");
        let seen = sink.seen.lock().unwrap().clone();
        assert_eq!(seen, agg.hook_attachments, "sink sees the same records");
        let a = &seen[0];
        assert_eq!(a["type"], "hook_success");
        assert_eq!(a["hookName"], "PostToolUse:Bash");
        assert_eq!(a["hookEvent"], "PostToolUse");
        assert_eq!(a["toolUseID"], "toolu_01ApkBwAZMCAza47B5nAWiGS");
        // Plain-text exit-0 arm: `content = jKe(stdout.trim())`.
        assert_eq!(a["content"], "formatted");
        assert_eq!(a["stdout"], "formatted\n");
        assert_eq!(a["stderr"], "");
        assert_eq!(a["exitCode"], 0);
        assert_eq!(a["command"], "./hooks/fmt.sh");
        assert!(a["durationMs"].is_u64(), "durationMs present: {a}");
        // Key ORDER is load-bearing.
        let keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "type",
                "hookName",
                "toolUseID",
                "hookEvent",
                "content",
                "stdout",
                "stderr",
                "exitCode",
                "command",
                "durationMs"
            ]
        );
    }

    #[tokio::test]
    async fn successful_large_output_is_persisted_and_replaced_by_a_reference() {
        let sink = Arc::new(RecordingSink::default());
        let output = "x".repeat(attachment::HOOK_OUTPUT_INLINE_LIMIT + 1);
        let exec = exec_with(Ok(out(&output, "", 0)), sink.clone());

        let agg = exec.execute(post_event(), HookContext::default()).await;

        assert_eq!(
            sink.large_outputs.lock().unwrap().as_slice(),
            [output.as_str()]
        );
        assert_eq!(
            agg.hook_attachments[0]["content"],
            "(Full output saved to: /session/tool-results/hook.txt)"
        );
        assert_eq!(agg.hook_attachments[0]["stdout"], output);
    }

    /// An engine without an async registry executes config-non-blocking hooks
    /// inline. Their decision remains excluded, but their real run record must
    /// not disappear.
    #[tokio::test]
    async fn non_blocking_inline_fallback_retains_run_attachment() {
        let mut hook = post_hook();
        hook.blocking = false;
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let sink = Arc::new(RecordingSink::default());
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_process_runner(
            Arc::new(FixedRunner(Mutex::new(Some(Ok(out(
                "background complete\n",
                "",
                0,
            )))))),
            Arc::new(StubSandbox),
        )
        .with_attachment_sink(sink.clone());

        let agg = exec.execute(post_event(), HookContext::default()).await;

        assert_eq!(agg.decision, None, "non-blocking hooks cannot gate a turn");
        assert_eq!(agg.hook_attachments.len(), 1);
        assert_eq!(sink.seen.lock().unwrap().as_slice(), agg.hook_attachments);
        assert_eq!(agg.hook_attachments[0]["content"], "background complete");
    }

    /// O2: the aggregate carries the blocking hook's `command` so the CALLER
    /// (the PostToolUse consumer) can build the `hook_blocking_error` record.
    ///
    /// The two blocking arms use DIFFERENT renderings, confirmed by reading the
    /// runner's own locals (`ee=qq(q)`, `te=iSe(q)` near BIN off 237802650):
    ///
    /// * plain-text **exit 2** (BIN off **237805098**) —
    ///   `{blockingError: `[${te}]: …`, command: te}`, i.e. `iSe`, the raw
    ///   per-arm rendering that NEVER consults `statusMessage`.
    /// * JSON **`decision:"block"`** (BIN off **237775430**, reached via
    ///   `Tfn({json, command: ee, …})`) — `{blockingError: reason || "Blocked
    ///   by hook", command: ee}`, i.e. `qq`, which prefers `statusMessage`.
    ///
    /// They coincide for a hook with no `statusMessage`, so the fixtures below
    /// SET one — otherwise the test could not tell the two apart.
    #[tokio::test]
    async fn exit_two_block_carries_the_ise_command_not_the_status_message() {
        let mut hook = post_hook();
        hook.status_message = Some("Formatting".into());
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_process_runner(
            Arc::new(FixedRunner(Mutex::new(Some(Ok(out("", "nope\n", 2)))))),
            Arc::new(StubSandbox),
        );

        let agg = exec.execute(post_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(
            agg.reason.as_deref(),
            Some("[./hooks/fmt.sh]: nope\n"),
            "the exit-2 `blockingError` STRING brackets the iSe display text"
        );
        assert_eq!(
            agg.block_command.as_deref(),
            Some("./hooks/fmt.sh"),
            "`command:te` is iSe — statusMessage must NOT win on this arm"
        );
    }

    #[tokio::test]
    async fn json_block_carries_the_qq_command_which_prefers_status_message() {
        let mut hook = post_hook();
        hook.status_message = Some("Formatting".into());
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_process_runner(
            Arc::new(FixedRunner(Mutex::new(Some(Ok(out(
                r#"{"decision":"block","reason":"unformatted"}"#,
                "",
                0,
            )))))),
            Arc::new(StubSandbox),
        );

        let agg = exec.execute(post_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert_eq!(agg.reason.as_deref(), Some("unformatted"));
        assert_eq!(
            agg.block_command.as_deref(),
            Some("Formatting"),
            "`command:ee` is qq — statusMessage wins on the JSON arm"
        );
    }

    /// O2: a hook that returns `systemMessage` publishes a SECOND attachment —
    /// `hook_system_message` — right after its run-outcome record.
    ///
    /// The oracle's runner emits the run-outcome `q.message` first, then the
    /// system-message payload on the same loop iteration (BIN off 237807875),
    /// so the transcript order is `[hook_success, hook_system_message]`.
    #[tokio::test]
    async fn system_message_publishes_a_second_attachment_after_the_run_record() {
        let sink = Arc::new(RecordingSink::default());
        let exec = exec_with(
            Ok(out(r#"{"systemMessage":"reformatted 3 files"}"#, "", 0)),
            sink.clone(),
        );

        let agg = exec.execute(post_event(), HookContext::default()).await;

        let seen = sink.seen.lock().unwrap().clone();
        assert_eq!(seen, agg.hook_attachments, "sink sees the same records");
        assert_eq!(
            seen.len(),
            2,
            "run-outcome record plus the system-message record, got {seen:?}"
        );
        assert_eq!(seen[0]["type"], "hook_success", "run outcome comes FIRST");
        assert_eq!(
            serde_json::to_string(&seen[1]).unwrap(),
            r#"{"type":"hook_system_message","content":"reformatted 3 files","hookName":"PostToolUse:Bash","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse"}"#
        );
    }

    /// The system-message record is emitted ONLY when the hook actually set a
    /// non-empty `systemMessage` — a plain hook still publishes exactly one.
    #[tokio::test]
    async fn no_system_message_means_no_second_attachment() {
        let sink = Arc::new(RecordingSink::default());
        let exec = exec_with(Ok(out(r#"{"systemMessage":""}"#, "", 0)), sink.clone());

        let agg = exec.execute(post_event(), HookContext::default()).await;

        assert_eq!(
            agg.hook_attachments.len(),
            1,
            "an EMPTY systemMessage is falsy in the oracle's `if(q.systemMessage)` guard"
        );
    }

    /// A JSON-returning hook takes claude's `Tfn` path, whose `hook_success`
    /// carries `content: ""` (all 25 901 mined records).
    #[tokio::test]
    async fn json_stdout_success_has_empty_content() {
        let sink = Arc::new(RecordingSink::default());
        let exec = exec_with(
            Ok(out(
                r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"hi"}}"#,
                "",
                0,
            )),
            sink.clone(),
        );

        exec.execute(post_event(), HookContext::default()).await;

        let seen = sink.seen.lock().unwrap().clone();
        assert_eq!(seen[0]["type"], "hook_success");
        assert_eq!(seen[0]["content"], "");
    }

    /// Any non-zero exit that is NOT the exit-2 block ⇒ `hook_non_blocking_error`.
    #[tokio::test]
    async fn non_zero_exit_publishes_hook_non_blocking_error() {
        let sink = Arc::new(RecordingSink::default());
        let exec = exec_with(Ok(out("partial", "boom", 1)), sink.clone());

        exec.execute(post_event(), HookContext::default()).await;

        let seen = sink.seen.lock().unwrap().clone();
        let a = &seen[0];
        let keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "type",
                "hookName",
                "toolUseID",
                "hookEvent",
                "stderr",
                "stdout",
                "exitCode",
                "command",
                "durationMs"
            ]
        );
        assert_eq!(a["type"], "hook_non_blocking_error");
        assert_eq!(a["stderr"], "boom");
        assert_eq!(a["stdout"], "partial");
        assert_eq!(a["exitCode"], 1);
    }

    /// exit 2 is claude's BLOCKING arm — it yields a `blockingError`, never a
    /// run-outcome attachment.
    #[tokio::test]
    async fn blocking_exit_two_publishes_no_run_attachment() {
        let sink = Arc::new(RecordingSink::default());
        let exec = exec_with(Ok(out("", "denied", 2)), sink.clone());

        let agg = exec.execute(post_event(), HookContext::default()).await;

        assert_eq!(agg.decision, Some(HookDecision::Block));
        assert!(
            agg.hook_attachments.is_empty(),
            "blocking runs carry hook_blocking_error, not a run attachment: {:?}",
            agg.hook_attachments
        );
        assert!(sink.seen.lock().unwrap().is_empty());
    }

    /// A hook that blew its deadline ⇒ `hook_cancelled` with `timedOut: true`
    /// and the declared `timeoutMs` (`re = q.timeout*1000`).
    #[tokio::test]
    async fn timed_out_run_publishes_hook_cancelled_with_timeout_pair() {
        let sink = Arc::new(RecordingSink::default());
        let exec = exec_with(Err(ProcessError::Timeout), sink.clone());

        exec.execute(post_event(), HookContext::default()).await;

        let seen = sink.seen.lock().unwrap().clone();
        let a = &seen[0];
        let keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "type",
                "hookName",
                "toolUseID",
                "hookEvent",
                "command",
                "durationMs",
                "timedOut",
                "timeoutMs"
            ]
        );
        assert_eq!(a["type"], "hook_cancelled");
        assert_eq!(a["timedOut"], true);
        assert_eq!(a["timeoutMs"], 90_000, "hook.timeout (90s) in ms");
    }

    /// Non-tool events mint a fresh uuid `toolUseID` and use the bare event
    /// name as `hookName`.
    #[tokio::test]
    async fn non_tool_event_mints_uuid_tool_use_id() {
        let mut hook = post_hook();
        hook.events = vec![HookEventType::Stop];
        let mut registry = HookRegistry::new();
        registry.register(hook);
        let sink = Arc::new(RecordingSink::default());
        let exec = HookExecutorImpl::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        )
        .with_process_runner(
            Arc::new(FixedRunner(Mutex::new(Some(Ok(out("", "", 0)))))),
            Arc::new(StubSandbox),
        )
        .with_attachment_sink(sink.clone());

        exec.execute(
            HookEvent::Stop {
                reason: "end_turn".into(),
            },
            HookContext::default(),
        )
        .await;

        let seen = sink.seen.lock().unwrap().clone();
        assert_eq!(seen[0]["hookName"], "Stop");
        let tuid = seen[0]["toolUseID"].as_str().unwrap();
        assert_eq!(tuid.len(), 36, "plain uuid, not a toolu_ id: {tuid}");
        let hyphens: Vec<usize> = tuid.match_indices('-').map(|(i, _)| i).collect();
        assert_eq!(hyphens, [8, 13, 18, 23], "uuid hyphenation: {tuid}");
        assert!(
            tuid.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
            "lowercase hex uuid: {tuid}"
        );
    }
}
