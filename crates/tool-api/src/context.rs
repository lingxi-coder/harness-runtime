//! Per-invocation tool context.
//!
//! [`ToolUseContext`] carries everything a tool needs about the surrounding
//! call: conversation history, the active tool-use id, agent id, options,
//! and shared state (content replacement, file-state cache in Plan 10).

use crate::content_replacement::ContentReplacementState;
use crate::registry::ToolRegistry;
use lingxi_core::host::audio::{
    AudioError, AudioErrorKind, AudioInitiator, AudioOperationContext, AudioOperationId, AudioOwner,
};
use lingxi_core::host::tool_invoker::ToolExecutionPolicy;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, AgentId, McpConnectionId, MessageId, SessionId, ToolUseId,
};
use lingxi_core::SessionState;
use media_api::{MediaOperationId, MediaSessionRef};
use std::sync::Arc;
use tokio::sync::Mutex;

/// How a host-originated call handles cancellation of its owned shell process.
/// Ordinary agent turns retain their existing interrupt/background behavior.
#[derive(Clone, Debug, Default)]
pub enum HostToolCancellation {
    /// Preserve the normal agent-turn cancellation behavior.
    #[default]
    Ordinary,
    /// Terminate and reap a shell owned by this individual host call.
    TerminateOwnedProcess(Arc<HostToolRequest>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum HostRequestPhase {
    #[default]
    Active,
    // The handoff snapshot is fixed, but the response has not yet been
    // flushed. Cancellation may still win; no new work may be admitted.
    Sealed,
    Cancelled,
    Committed,
}

struct HostBackgroundProcess {
    runner: Arc<dyn lingxi_core::host::ProcessRunner>,
    handle: lingxi_core::host::ProcessHandle,
    owner: String,
    ready: bool,
    output_cleanup: Option<(
        Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
        String,
    )>,
    sandbox_lease: Option<crate::sandbox_runner::SandboxLease>,
}

#[derive(Default)]
struct HostRequestState {
    phase: HostRequestPhase,
    pending: usize,
    cleaning: usize,
    backgrounds: Vec<HostBackgroundProcess>,
    errors: Vec<String>,
    cleanup_started: bool,
    cleanup_done: bool,
}

// A request can end after reporting an unverified cleanup. Keeping the lease
// on the request alone would release resources while that process might still
// be using them. Quarantine survives request and runtime task cancellation;
// it is released only by a holder with a verified termination receipt.
type QuarantinedLeases =
    std::sync::Mutex<std::collections::HashMap<String, crate::sandbox_runner::SandboxLease>>;
fn quarantined_leases() -> &'static QuarantinedLeases {
    static LEASES: std::sync::OnceLock<QuarantinedLeases> = std::sync::OnceLock::new();
    LEASES.get_or_init(Default::default)
}

/// A lease retained independently of its originating request.
/// Dropping this handle does not release resources: an unverified process
/// must not cause cleanup merely because its request or waiter disappeared.
pub struct QuarantinedSandboxLease(Option<String>);

impl QuarantinedSandboxLease {
    /// Quarantine resources until a caller can prove process termination.
    #[must_use]
    pub fn new(lease: crate::sandbox_runner::SandboxLease) -> Self {
        if lease.is_noop() {
            return Self(None);
        }
        loop {
            let id = lingxi_core::types::ToolUseId::new().to_string();
            let mut leases = quarantined_leases()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let std::collections::hash_map::Entry::Vacant(slot) = leases.entry(id.clone()) {
                slot.insert(lease);
                return Self(Some(id));
            }
        }
    }

    /// Release after the process runner has provided a verified terminal result.
    pub fn release_verified(mut self) {
        if let Some(id) = self.0.take() {
            let lease = {
                let mut leases = quarantined_leases()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                leases.remove(&id)
            };
            // Cleanup callbacks can perform arbitrary work, so run them after
            // releasing the quarantine lock.
            drop(lease);
        }
    }
}

/// Tracks background handoffs until a host request is cancelled or committed.
#[derive(Default)]
pub struct HostToolRequest {
    state: Arc<std::sync::Mutex<HostRequestState>>,
    changed: Arc<tokio::sync::Notify>,
    cancel_signal: tokio_util::sync::CancellationToken,
    lifecycle_signal: tokio_util::sync::CancellationToken,
    owner_id: lingxi_core::types::ToolUseId,
}

/// One durable mutation admitted by an active host request. The lease is
/// movable across tasks but cannot be cloned; it remains pending until drop.
pub struct HostRequestMutationLease {
    request: Arc<HostToolRequest>,
    completed: bool,
}

impl HostRequestMutationLease {
    /// Mark the admitted mutation complete before releasing its pending slot.
    /// Dropping without this acknowledgement records an unverified interruption.
    pub fn complete(mut self) {
        self.completed = true;
    }
}

impl Drop for HostRequestMutationLease {
    fn drop(&mut self) {
        let mut state = self.request.state();
        state.pending -= 1;
        if !self.completed {
            state
                .errors
                .push("host request mutation was interrupted; completion is unverified".into());
        }
        drop(state);
        self.request.changed.notify_waiters();
    }
}

// Once a response's task-id snapshot has been sealed, the client may receive
// it even if its final write remains in flight. Keep ownership until delivery
// or cancellation is known. This hold lasts only as long as the host process;
// it cannot provide persistence after that process exits.
fn undecided_handoffs(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, Arc<HostToolRequest>>> {
    static HANDOFFS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Arc<HostToolRequest>>>,
    > = std::sync::OnceLock::new();
    HANDOFFS.get_or_init(Default::default)
}

impl std::fmt::Debug for HostToolRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostToolRequest").finish_non_exhaustive()
    }
}

// A verified process may exit while its detached registration future is still
// publishing a task row. Keep the allocation and its early receipt until every
// admitted launch has completed; then the registry can decide whether the
// allocation is still redundant.
async fn discard_after_pending_launches(
    state: Arc<std::sync::Mutex<HostRequestState>>,
    changed: Arc<tokio::sync::Notify>,
    registry: Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
    id: String,
) {
    loop {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            == 0
        {
            break;
        }
        notified.await;
    }
    registry.discard_bash_output(&id).await;
}

impl HostToolRequest {
    fn state(&self) -> std::sync::MutexGuard<'_, HostRequestState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn release_handoff_hold(&self) {
        let held = {
            let mut handoffs = undecided_handoffs()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handoffs.remove(&self.owner_id.to_string())
        };
        drop(held);
    }

    /// Mark cancellation before scheduling asynchronous cleanup.
    pub fn cancel(&self) -> bool {
        let mut state = self.state();
        if !matches!(
            state.phase,
            HostRequestPhase::Active | HostRequestPhase::Sealed
        ) {
            return false;
        }
        state.phase = HostRequestPhase::Cancelled;
        self.cancel_signal.cancel();
        self.lifecycle_signal.cancel();
        drop(state);
        self.release_handoff_hold();
        self.changed.notify_waiters();
        self.start_cleanup();
        true
    }

    /// A unique owner shared by all shells belonging to this request.
    pub fn process_owner(&self) -> String {
        format!("host-tool:{}", self.owner_id)
    }

    /// Request-level cancellation also applies when no separate token was supplied.
    pub fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel_signal.clone()
    }

    /// Whether this request can still publish or admit nested work.
    pub fn is_active(&self) -> bool {
        self.state().phase == HostRequestPhase::Active
    }

    /// Admit a durable mutation atomically with cancellation and handoff.
    /// Once admitted, keep the lease until the mutation has finished; request
    /// cleanup waits for it even if cancellation wins in the meantime.
    pub fn try_admit_mutation(self: &Arc<Self>) -> Option<HostRequestMutationLease> {
        let mut state = self.state();
        if state.phase != HostRequestPhase::Active {
            return None;
        }
        state.pending += 1;
        Some(HostRequestMutationLease {
            request: self.clone(),
            completed: false,
        })
    }

    /// Signalled when the request stops admitting work, on handoff or cancellation.
    pub fn lifecycle_token(&self) -> tokio_util::sync::CancellationToken {
        self.lifecycle_signal.clone()
    }

    /// Commit a completed request. A cancellation that won first cannot commit.
    pub fn commit(&self) -> bool {
        let mut state = self.state();
        if state.phase != HostRequestPhase::Sealed
            || state.pending != 0
            || state.cleaning != 0
            || !state.errors.is_empty()
            || state.backgrounds.iter().any(|entry| !entry.ready)
        {
            return false;
        }
        state.phase = HostRequestPhase::Committed;
        self.lifecycle_signal.cancel();
        let transferred = std::mem::take(&mut state.backgrounds);
        drop(state);
        self.release_handoff_hold();
        drop(transferred);
        true
    }

    /// Whether the host has cancelled this request.
    pub fn is_cancelled(&self) -> bool {
        self.state().phase == HostRequestPhase::Cancelled
    }

    /// Retain a background process until the host commits or cancels the call.
    pub fn track_background(
        &self,
        runner: Arc<dyn lingxi_core::host::ProcessRunner>,
        handle: lingxi_core::host::ProcessHandle,
        owner: String,
    ) {
        self.track_background_with_cleanup(runner, handle, owner, None, None);
    }

    /// Retain a handle and discard its unused allocation only after verified reap.
    ///
    /// A launched handle must be tracked within the `run_background` future
    /// which launched it. Its pending guard prevents a handoff snapshot until
    /// registration and acknowledgement have finished.
    pub fn track_background_with_cleanup(
        &self,
        runner: Arc<dyn lingxi_core::host::ProcessRunner>,
        handle: lingxi_core::host::ProcessHandle,
        owner: String,
        output_cleanup: Option<(
            Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>,
            String,
        )>,
        sandbox_lease: Option<crate::sandbox_runner::SandboxLease>,
    ) {
        let mut state = self.state();
        if state.phase != HostRequestPhase::Committed {
            state.backgrounds.push(HostBackgroundProcess {
                runner,
                handle,
                owner,
                ready: false,
                output_cleanup,
                sandbox_lease,
            });
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// Mark a tracked handoff usable only after registry binding and supervisor
    /// acknowledgement have both completed. Cancellation may have won first.
    pub fn mark_background_ready(&self, handle: &lingxi_core::host::ProcessHandle) -> bool {
        let mut state = self.state();
        if state.phase != HostRequestPhase::Active {
            return false;
        }
        if let Some(entry) = state
            .backgrounds
            .iter_mut()
            .find(|entry| entry.handle.task_id == handle.task_id && entry.handle.pid == handle.pid)
        {
            entry.ready = true;
            true
        } else {
            false
        }
    }

    /// Atomically seal admission and snapshot the usable handoff ids.
    ///
    /// The caller can then serialize and flush these exact ids. Cancellation
    /// remains possible until `commit`; no new owned operation can start after
    /// this returns successfully. Repeated reads of the sealed snapshot are safe.
    pub fn handoff_task_ids(self: &Arc<Self>) -> Result<Vec<String>, String> {
        let mut state = self.state();
        if !matches!(
            state.phase,
            HostRequestPhase::Active | HostRequestPhase::Sealed
        ) || state.pending != 0
            || state.cleaning != 0
            || !state.errors.is_empty()
            || state.backgrounds.iter().any(|entry| !entry.ready)
        {
            return Err("owned process handoff is not ready".into());
        }
        if state.phase == HostRequestPhase::Active {
            state.phase = HostRequestPhase::Sealed;
            self.lifecycle_signal.cancel();
            undecided_handoffs()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(self.owner_id.to_string())
                .or_insert_with(|| self.clone());
        }
        Ok(state
            .backgrounds
            .iter()
            .map(|entry| entry.handle.task_id.clone())
            .collect())
    }

    /// Keep sandbox resources independently of this request when termination
    /// cannot yet be verified. The returned handle can release them only after
    /// the caller obtains a verified terminal result.
    pub fn retain_sandbox_lease(
        &self,
        lease: crate::sandbox_runner::SandboxLease,
    ) -> QuarantinedSandboxLease {
        QuarantinedSandboxLease::new(lease)
    }

    /// Preserve evidence that a launched process could not be verified as reaped.
    pub fn record_cleanup_error(&self, error: String) {
        self.state().errors.push(error);
        self.changed.notify_waiters();
    }

    /// Run a background launch independently of the calling future, so an EOF
    /// cannot drop the only owner while the process handshake is still pending.
    pub async fn run_background<F, T>(
        self: &Arc<Self>,
        future: F,
    ) -> Result<Option<T>, tokio::task::JoinError>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        {
            let mut state = self.state();
            if state.phase != HostRequestPhase::Active {
                return Ok(None);
            }
            state.pending += 1;
        }
        struct Pending(Arc<HostToolRequest>);
        impl Drop for Pending {
            fn drop(&mut self) {
                let mut state = self.0.state();
                state.pending -= 1;
                if std::thread::panicking() {
                    state
                        .errors
                        .push("owned process task panicked; cleanup is unverified".into());
                }
                drop(state);
                self.0.changed.notify_waiters();
            }
        }
        let pending = Pending(self.clone());
        tokio::spawn(async move {
            let _pending = pending;
            future.await
        })
        .await
        .map(Some)
    }

    // The driver owns each handle independently of any caller waiting for it.
    // It continues handling late registrations even when a waiter is aborted.
    fn start_cleanup(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let mut state = self.state();
            if state.phase != HostRequestPhase::Cancelled || state.cleanup_started {
                return;
            }
            state.cleanup_started = true;
        }
        let state = self.state.clone();
        let changed = self.changed.clone();
        runtime.spawn(async move {
            loop {
                let notified = changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let (backgrounds, done) = {
                    let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    let backgrounds = std::mem::take(&mut state.backgrounds);
                    state.cleaning += backgrounds.len();
                    let done = state.pending == 0 && state.cleaning == 0;
                    if done { state.cleanup_done = true; }
                    (backgrounds, done)
                };
                if done {
                    changed.notify_waiters();
                    return;
                }
                // Start every stop concurrently. A hung first process must not
                // prevent later owned processes from receiving a stop request.
                for background in backgrounds {
                    let state = state.clone();
                    let changed = changed.clone();
                    tokio::spawn(async move {
                        let task_id = background.handle.task_id.clone();
                        // Quarantine before spawning so a panic, timed-out
                        // waiter, or dropped runtime task cannot release it.
                        let lease = background.sandbox_lease.map(QuarantinedSandboxLease::new);
                        let pending_state = state.clone();
                        let pending_changed = changed.clone();
                        let stopping = tokio::spawn(async move {
                            let result = background.runner.terminate_and_wait_owned_background(
                                &background.handle, &background.owner,
                            ).await;
                            if result.is_ok() {
                                if let Some(lease) = lease { lease.release_verified(); }
                                if let Some((registry, id)) = background.output_cleanup {
                                    discard_after_pending_launches(pending_state, pending_changed, registry, id).await;
                                }
                            } else {
                                // The original caller receives the failure, but
                                // ownership survives it and retries until a real
                                // termination receipt is available.
                                tokio::spawn(async move {
                                    loop {
                                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                                        if background.runner.terminate_and_wait_owned_background(
                                            &background.handle, &background.owner,
                                        ).await.is_ok() {
                                            if let Some(lease) = lease { lease.release_verified(); }
                                            if let Some((registry, id)) = background.output_cleanup {
                                                discard_after_pending_launches(pending_state, pending_changed, registry, id).await;
                                            }
                                            break;
                                        }
                                    }
                                });
                            }
                            result
                        });
                        let result = match tokio::time::timeout(std::time::Duration::from_secs(20), stopping).await {
                            Ok(Ok(result)) => result,
                            Ok(Err(error)) => Err(mobile_linux_api::ProcessError::Io(format!(
                                "process {task_id} cleanup task failed: {error}"
                            ))),
                            Err(_) => Err(mobile_linux_api::ProcessError::Io(format!(
                                "process {task_id} did not produce a termination receipt before the cleanup deadline"
                            ))),
                        };
                        {
                            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.cleaning -= 1;
                            if let Err(error) = result {
                                state.errors.push(error.to_string());
                            }
                        }
                        changed.notify_waiters();
                    });
                }
                let complete = {
                    let state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.pending == 0 && state.cleaning == 0
                };
                if !complete { notified.await; }
            }
        });
    }

    /// Cancel every uncommitted handoff and wait for verified process termination.
    /// A missing registration is never accepted as proof of reaping.
    pub async fn cancel_and_reap(&self) -> Result<(), String> {
        self.cancel_and_reap_with_timeout(std::time::Duration::from_secs(30))
            .await
    }

    async fn cancel_and_reap_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        self.cancel();
        self.start_cleanup();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state();
                if state.phase == HostRequestPhase::Committed {
                    return Ok(());
                }
                if state.cleanup_done {
                    return if state.errors.is_empty() {
                        Ok(())
                    } else {
                        Err(state.errors.join("; "))
                    };
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let mut state = self.state();
                let error = format!(
                    "owned process cleanup timed out with {} launch(es) and {} cleanup(s) pending; termination is unverified",
                    state.pending, state.cleaning,
                );
                state.errors.push(error);
                return Err(state.errors.join("; "));
            }
        }
    }
}

tokio::task_local! {
    static HOST_TOOL_CANCELLATION: HostToolCancellation;
    static PROCESS_EXECUTION_LEASE: Option<Arc<dyn Send + Sync>>;
}

/// Carry the originating turn's execution lease through a tool invocation.
/// A process runner must clone it into work that can outlive the caller.
pub async fn with_process_execution_lease<F: std::future::Future>(
    lease: Option<Arc<dyn Send + Sync>>,
    future: F,
) -> F::Output {
    PROCESS_EXECUTION_LEASE.scope(lease, future).await
}

/// Capture the current invocation's execution lease before starting a process.
#[must_use]
pub fn process_execution_lease() -> Option<Arc<dyn Send + Sync>> {
    PROCESS_EXECUTION_LEASE
        .try_with(Clone::clone)
        .ok()
        .flatten()
}

/// Apply a host cancellation policy only while polling this call's future.
/// The scope is task-local, so concurrently dispatched calls remain independent.
pub async fn with_host_tool_cancellation<F: std::future::Future>(
    policy: HostToolCancellation,
    future: F,
) -> F::Output {
    HOST_TOOL_CANCELLATION.scope(policy, future).await
}

/// Return the calling future's host policy, or ordinary turn behavior.
pub fn host_tool_cancellation() -> HostToolCancellation {
    HOST_TOOL_CANCELLATION
        .try_with(Clone::clone)
        .unwrap_or_default()
}

/// Per-invocation context passed to every [`crate::Tool`] method that needs
/// to reason about the current call (validation, permission, execution).
///
/// Cloning is cheap: most fields are `Arc`-shared or `Copy`. The file-state
/// cache slot lands in Plan 10.
#[derive(Clone)]
pub struct ToolUseContext {
    /// Static per-call options (debug flags, budget, system-prompt overrides).
    pub options: ToolUseOptions,
    /// Conversation history up to (but not including) the current call.
    pub messages: Vec<lingxi_core::types::ConversationMessage>,
    /// The tool-use id assigned by the model, if this call is bound to one.
    pub tool_use_id: Option<ToolUseId>,
    /// The current assistant message that owns this tool-use batch, if known.
    /// Main-loop dispatch sets this to the live assistant turn id so tools and
    /// telemetry hooks can attribute validation failures to the current
    /// request. Omitted for call sites that have no current assistant message
    /// (for example isolated unit tests and subagent helper invocations).
    pub assistant_message_id: Option<MessageId>,
    /// Raw assistant row for the currently dispatched completed block. This
    /// is independent of `messages` (the pre-query history snapshot) and is
    /// refreshed from trusted host facts for every nested invocation.
    pub assistant_message: Option<lingxi_core::types::ConversationMessage>,
    /// Tool-use blocks from earlier sibling rows in the same query, excluding
    /// the current assistant row. The host refreshes this on every call.
    pub same_turn_tool_uses: Vec<lingxi_core::types::ContentBlock>,
    /// Effective tool names visible to the model query that produced this call,
    /// including registered aliases. `None` means the caller did not supply a
    /// tool list; `Some([])` is an explicitly empty caller list.
    pub available_tool_names: Option<Vec<Utf16JsonProjection>>,
    /// The agent that issued this call, if known.
    pub agent_id: Option<AgentId>,
    /// Native per-context nested-memory triggers; independent of file cache.
    pub nested_memory_triggers: Arc<crate::nested_memory_triggers::NestedMemoryTriggers>,
    /// The DISPLAY NAME of the teammate that issued this call, if known
    /// (claude-code `getAgentName()`). For an in-process teammate this is its
    /// human name (e.g. `"researcher"`), NOT the `agent:<uuid>` form of
    /// [`Self::agent_id`]. `None` for the main thread / leader (TS
    /// `getAgentName()` returns `undefined`). The swarm-only `TaskUpdate`
    /// side-effects (auto-owner, owner-change mailbox notification) key on this
    /// NAME so on-disk owners / mailbox senders match what `getAgentStatuses`
    /// looks up (name, never `agent:<uuid>`).
    pub agent_name: Option<String>,
    /// The TEAM NAME the issuing teammate belongs to, if known (claude-code
    /// `getTeammateContext()?.teamName`). Consulted by `getTaskListId()`
    /// (priority 2) so in-process teammates resolve to the leader's on-disk task
    /// directory. `None` for the main thread / standalone sessions.
    pub team_name: Option<String>,
    /// Trusted session that originated a nested subagent tool invocation.
    /// Main-loop calls normally leave this unset because [`Self::session`]
    /// exposes the live session directly. Subagent dispatch has no writable
    /// session state, so recursive Agent, Fusion, and Workflow calls use this
    /// immutable identity to keep their cost and budget accounting attached to
    /// the originating conversation.
    pub origin_session_id: Option<SessionId>,
    /// The host-captured originating session capability, carried explicitly to
    /// nested agent launches. It is never derived from tool input or UUID alone.
    pub session_origin: Option<Arc<dyn lingxi_core::host::subagent_spawn::SessionOriginCapability>>,
    /// Host-captured plugin caller/origin for nested `agent.spawn` events.
    /// Mod-rewritten JSON never supplies or overwrites this identity.
    pub agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    /// Current Native query-function depth for a main-loop tool invocation.
    /// None means this call was not produced by a tracked model query.
    pub native_query_depth: Option<u32>,
    /// Trusted host origin for this turn. Scheduled turns cannot transfer
    /// ownership to interactive-only continuations such as media realtime.
    pub is_scheduled_turn: bool,
    /// Trusted host-selected execution policy for this invocation. The nested
    /// dispatch path copies it from `SubagentInvocationContext`; ordinary
    /// callers use [`ToolExecutionPolicy::Ordinary`].
    pub tool_execution_policy: ToolExecutionPolicy,
    /// Trusted effective permission mode for recursive Agent launches. Nested
    /// dispatch derives this from its host override or the enforcing live gate.
    pub trusted_effective_permission_mode: Option<String>,
    /// Invocation-local verdict from the bound classifier-only permission
    /// path. Tool bodies may annotate reports but cannot mint this result.
    pub classifier_only_review: Option<lingxi_core::host::handback::ReportReview>,
    /// Host snapshot inherited by ordinary and forked children.
    pub instruction_context: Option<lingxi_core::host::instructions::InstructionContext>,
    /// Shared content-replacement state. Populated in Task 3.
    pub content_replacement_state: Option<Arc<Mutex<ContentReplacementState>>>,
    /// Mutable session state (M4-04). Tools that mutate the conversation
    /// (`TodoWrite`, `EnterPlanMode`, `ExitPlanMode`) acquire the `Mutex`
    /// before reading or writing. `None` for legacy call sites that have not
    /// wired a session yet; M4-04 tools surface a clear error in that case.
    pub session: Option<Arc<Mutex<SessionState>>>,
    /// Parent's tool registry — `AgentTool` clones this `Arc` into the child
    /// subagent's invocation context so the recursion lock (parent and child
    /// share the same `Arc<ToolRegistry>`) is asserted via `Arc::ptr_eq` in
    /// M4-05 wiring tests. `None` for top-level invocations that have no
    /// parent agent yet.
    pub subagent_registry: Option<Arc<ToolRegistry>>,
    /// Per-call cancellation token. Fired by the streaming executor when a
    /// sibling Bash tool errors (or the user interrupts). Phase 1 carries it;
    /// the Bash/subprocess tools observe it in Phase 2 to kill in-flight work.
    /// `None` for batched/legacy call sites.
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    /// Trusted host cancellation behavior for this invocation. Kept in the
    /// context so a tool retains its policy when its future moves to a new task.
    pub host_tool_cancellation: HostToolCancellation,
    /// The parent agent's already-rendered system prompt bytes, threaded onto a
    /// fork-subagent spawn so the child replays the parent's exact prompt (TS
    /// `forkContextMessages` / `override.systemPrompt = forkParentSystemPrompt`).
    /// `None` for the non-fork path and for call sites the orchestrator has not
    /// yet wired (`AgentTool` then runs the fork child with `FORK_AGENT`'s empty
    /// system prompt — functional, not byte-identical).
    pub fork_parent_system_prompt: Option<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Per-agent working directory OVERRIDE (claude-code's `agentWorktree` /
    /// `cwd`, threaded into the agent's `AsyncLocalStorage` cwd that every tool
    /// reads via `Mt()`). `Some` ONLY for a subagent isolated in a git worktree
    /// (`isolation:"worktree"`) or given an explicit `cwd` — the dispatch invoker
    /// populates it from [`crate::SubagentInvocationContext::cwd`]. Filesystem and
    /// shell tools resolve their base directory from this when present, else from
    /// the shared session [`crate::BuiltinToolContext::workspace`]. `None` for the
    /// main thread and every non-isolated call (byte-identical to before).
    pub cwd: Option<std::path::PathBuf>,
    /// This agent's recursion depth — claude's `agentContext.depth` (`z6`:
    /// `"main"` ⇒ 0, else this value). The `Agent` tool reads it to set a
    /// spawned child's depth (`child = depth + 1`), and the subagent
    /// tool-resolver applies `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH` (default 1).
    /// `0` for the main thread and every non-subagent call; the dispatch invoker
    /// overwrites it from [`crate::SubagentInvocationContext::depth`] for a
    /// subagent's own tool calls.
    pub depth: u32,
    /// Effective observer inherited from a parent subagent. The Agent tool
    /// copies this to a child only when the child has no direct observer.
    pub observer: Option<lingxi_core::host::subagent_spawn::ObserverSpec>,
    /// The session's observer PAIRINGS (oracle `toolUseContext.session
    /// .observers`). Distinct from [`Self::observer`], which is the
    /// declaration propagated to children; this is the live observed↔observer
    /// table an `ObserverReport` resolves its destination from, and the table
    /// `SendMessage` consults to refuse an observer. `None` for hosts that
    /// have not wired observers — the tool then refuses with the oracle's
    /// "not armed" wording rather than pretending a pairing exists.
    pub observer_pairings: Option<Arc<lingxi_core::host::observer_pairing::ObserverPairings>>,
    /// (`/rewind`) Pre-edit file-history backup hook. The `Edit`/`Write`/
    /// `NotebookEdit` tools call `track_edit(path)` through this BEFORE writing,
    /// so `/rewind` can restore the pre-edit content. `None` (tests / no
    /// checkpointing) makes every write untracked (no behavior change).
    pub file_history: Option<Arc<dyn lingxi_core::host::FileHistorySink>>,
    // File state cache wired in Plan 10.
}

/// Typed, host-bound continuation request for a media conversation session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAgentHandoffRequest {
    /// Host-minted identity of the successful session-start operation.
    pub operation_id: MediaOperationId,
    /// Opaque reference returned by the host media service.
    pub session: MediaSessionRef,
    /// Trusted mode selected for the media session. Host handoff admission
    /// checks this before allowing the session to continue the Agent turn.
    pub mode: media_api::MediaSessionMode,
}

/// Trusted model route information for deciding whether a media session can
/// continue through the current Agent's selected provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAgentCapabilityContext {
    /// Current host-bound conversation session.
    pub session_id: String,
    /// Current model identifier.
    pub model: String,
    /// Exact profile pinned to this session, if present.
    pub profile_name: Option<String>,
    /// Account scope derived from the pinned profile.
    pub account_scope: Option<String>,
}

/// Narrow callback that asks the current turn owner to continue as an Agent
/// media conversation after the ordinary tool result has been committed.
///
/// Implementations must bind the request to trusted invocation context and
/// return promptly. They must not reserve a second turn or start an Agent run
/// reentrantly from inside a tool call.
pub trait MediaAgentHandoffHandle: Send + Sync {
    /// Structural modes that may be offered by this host attachment. The
    /// default supports audio-only; video modes require an explicit host
    /// implementation. Invocation-specific checks still run at dispatch.
    fn supported_modes(&self) -> Vec<media_api::MediaSessionMode> {
        vec![media_api::MediaSessionMode::Audio]
    }

    /// Modes advertised for this exact session and provider route. Hosts with
    /// connection-scoped provider bindings should resolve them by the supplied
    /// trusted context instead of exposing a process-wide capability union.
    fn supported_modes_for_context(
        &self,
        context: &MediaAgentCapabilityContext,
    ) -> Vec<media_api::MediaSessionMode> {
        self.supported_modes()
            .into_iter()
            .filter(|mode| self.supports_mode(context, *mode))
            .collect()
    }

    /// Whether this exact session profile can attach the selected media mode.
    fn supports_mode(
        &self,
        _context: &MediaAgentCapabilityContext,
        mode: media_api::MediaSessionMode,
    ) -> bool {
        self.supported_modes().contains(&mode)
    }

    /// Queue a deferred handoff for this trusted main-loop invocation.
    fn request(
        &self,
        invocation: &ToolUseContext,
        capability_context: &MediaAgentCapabilityContext,
        request: MediaAgentHandoffRequest,
    ) -> Result<(), String>;
}

impl ToolUseContext {
    /// Snapshot trusted media routing identity from the exact current
    /// conversation state. This awaits the session lock so normal lock
    /// contention does not masquerade as an unsupported provider.
    pub async fn media_agent_capability_context(&self) -> Option<MediaAgentCapabilityContext> {
        let session = self.session.as_ref()?.lock().await;
        // ToolUseOptions is captured by the owning request builder. Prefer its
        // resolved route profile so a trusted default route remains usable
        // even when the session did not persist an explicit profile pin.
        let profile_name = self
            .options
            .model_profile
            .clone()
            .or_else(|| session.model_profile.clone());
        Some(MediaAgentCapabilityContext {
            session_id: session.session_id.as_uuid().to_string(),
            model: session.model.clone(),
            account_scope: profile_name
                .as_ref()
                .map(|profile| format!("profile:{profile}")),
            profile_name,
        })
    }

    /// Build device-audio ownership only from trusted host context. The
    /// per-call agent/tool IDs remain attribution and never become the stable
    /// recording owner.
    pub async fn audio_operation_context(
        &self,
        identity: AudioOperationId,
        timeout_budget_ms: Option<u64>,
        max_payload_bytes: u64,
    ) -> Result<AudioOperationContext, AudioError> {
        let session_id = if let Some(origin_session_id) = self.origin_session_id.as_ref() {
            origin_session_id.to_string()
        } else if let Some(session) = self.session.as_ref() {
            session.lock().await.session_id.to_string()
        } else {
            return Err(AudioError::new(
                AudioErrorKind::InvalidRequest,
                "audio call has no trusted session owner",
            ));
        };

        let initiator = AudioInitiator {
            agent_id: self.agent_id.as_ref().map(ToString::to_string),
            tool_use_id: self.tool_use_id.as_ref().map(ToString::to_string),
            request_id: None,
        };

        Ok(AudioOperationContext {
            identity,
            owner: AudioOwner::Session { session_id },
            initiator: (initiator.agent_id.is_some() || initiator.tool_use_id.is_some())
                .then_some(initiator),
            timeout_budget_ms,
            max_payload_bytes,
        })
    }

    /// Build a minimal context that carries ONLY `main_loop_model`; every other
    /// field is inert (`None` / empty / default).
    ///
    /// The turn loop seeds this with the live `session.model`, folds a tool
    /// batch's [`crate::tool_trait::ContextModifier`]s over it, and reads back
    /// the resolved `options.main_loop_model` to apply a skill's `model:`
    /// override (SKILLEXEC.3, model scope). It is never handed to a tool, so the
    /// inert fields are never observed.
    #[must_use]
    pub fn model_seed(main_loop_model: String) -> Self {
        Self {
            options: ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model,
                model_profile: None,
                max_budget_nano_usd: None,
                mcp_clients: Vec::new(),
                is_non_interactive_session: true,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            messages: Vec::new(),
            tool_use_id: None,
            assistant_message_id: None,
            available_tool_names: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            agent_id: None,
            nested_memory_triggers: Arc::default(),
            agent_name: None,
            team_name: None,
            origin_session_id: None,
            session_origin: None,
            agent_spawn_provenance: Default::default(),
            native_query_depth: None,
            is_scheduled_turn: false,
            instruction_context: None,
            tool_execution_policy: ToolExecutionPolicy::Ordinary,
            trusted_effective_permission_mode: None,
            classifier_only_review: None,
            content_replacement_state: None,
            session: None,
            subagent_registry: None,
            cancel: None,
            host_tool_cancellation: HostToolCancellation::Ordinary,
            fork_parent_system_prompt: None,
            cwd: None,
            // Inert seed; model-only context is never handed to a tool.
            depth: 0,
            observer: None,
            observer_pairings: None,
            file_history: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn process_execution_lease_is_scoped_and_not_implicitly_inherited() {
        let lease: std::sync::Arc<dyn Send + Sync> = std::sync::Arc::new(());
        assert!(super::process_execution_lease().is_none());
        super::with_process_execution_lease(Some(lease.clone()), async {
            assert!(std::sync::Arc::ptr_eq(
                &super::process_execution_lease().unwrap(),
                &lease
            ));
            assert!(
                tokio::spawn(async { super::process_execution_lease().is_none() })
                    .await
                    .unwrap()
            );
            super::with_process_execution_lease(None, async {
                assert!(super::process_execution_lease().is_none());
            })
            .await;
            assert!(std::sync::Arc::ptr_eq(
                &super::process_execution_lease().unwrap(),
                &lease
            ));
        })
        .await;
        assert!(super::process_execution_lease().is_none());
    }
    use super::*;

    struct CleanupRunner {
        stopped: Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl lingxi_core::host::ProcessRunner for CleanupRunner {
        async fn run(
            &self,
            _: &lingxi_core::host::SandboxedCommand,
        ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError> {
            unreachable!()
        }
        async fn spawn_background(
            &self,
            _: &lingxi_core::host::SandboxedCommand,
        ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError> {
            unreachable!()
        }
        async fn kill(
            &self,
            _: &lingxi_core::host::ProcessHandle,
        ) -> Result<(), mobile_linux_api::ProcessError> {
            unreachable!()
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn terminate_and_wait_owned_background(
            &self,
            _: &lingxi_core::host::ProcessHandle,
            _: &str,
        ) -> Result<(), mobile_linux_api::ProcessError> {
            self.stopped
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                Err(mobile_linux_api::ProcessError::Io(
                    "unverified cleanup".into(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn tool_use_context_carries_optional_cancel_token() {
        let mut ctx = ToolUseContext::model_seed("opus".into());
        assert!(ctx.cancel.is_none());
        ctx.cancel = Some(tokio_util::sync::CancellationToken::new());
        assert!(ctx.cancel.is_some());
    }

    #[tokio::test]
    async fn host_cancellation_scope_is_per_future() {
        let request = Arc::new(HostToolRequest::default());
        let (strict, ordinary) = tokio::join!(
            with_host_tool_cancellation(
                HostToolCancellation::TerminateOwnedProcess(request.clone()),
                async {
                    tokio::task::yield_now().await;
                    host_tool_cancellation()
                }
            ),
            async {
                tokio::task::yield_now().await;
                host_tool_cancellation()
            }
        );
        assert!(
            matches!(strict, HostToolCancellation::TerminateOwnedProcess(actual) if Arc::ptr_eq(&actual, &request))
        );
        assert!(matches!(ordinary, HostToolCancellation::Ordinary));
        assert!(matches!(
            host_tool_cancellation(),
            HostToolCancellation::Ordinary
        ));
    }

    #[tokio::test]
    async fn host_background_cancel_waits_for_detached_pending_launch_on_eof() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let request = Arc::new(HostToolRequest::default());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let stopped = Arc::new(AtomicUsize::new(0));
        let runner = Arc::new(CleanupRunner {
            stopped: stopped.clone(),
            fail: false,
        });
        let launch_request = request.clone();
        let launch = tokio::spawn({
            let started = started.clone();
            let release = release.clone();
            async move {
                let track = launch_request.clone();
                launch_request
                    .run_background(async move {
                        started.notify_one();
                        release.notified().await;
                        track.track_background(
                            runner,
                            lingxi_core::host::ProcessHandle {
                                task_id: "test".into(),
                                pid: 42,
                            },
                            "host-tool:test".into(),
                        );
                    })
                    .await
            }
        });
        started.notified().await;
        // Simulates the stdio JoinSet deadline dropping its request task.
        launch.abort();
        assert!(request.cancel());
        let cleanup = tokio::spawn({
            let request = request.clone();
            async move { request.cancel_and_reap().await }
        });
        tokio::task::yield_now().await;
        assert!(!cleanup.is_finished(), "pending launch must remain owned");
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), cleanup)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert!(!request.commit(), "cancel won before completion");
    }

    #[tokio::test]
    async fn host_cleanup_survives_aborted_waiter_and_starts_all_handles() {
        use std::time::Duration;
        struct Runner {
            started: tokio::sync::mpsc::UnboundedSender<String>,
            release: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl lingxi_core::host::ProcessRunner for Runner {
            async fn run(
                &self,
                _: &lingxi_core::host::SandboxedCommand,
            ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError>
            {
                unreachable!()
            }
            async fn spawn_background(
                &self,
                _: &lingxi_core::host::SandboxedCommand,
            ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError>
            {
                unreachable!()
            }
            async fn kill(
                &self,
                _: &lingxi_core::host::ProcessHandle,
            ) -> Result<(), mobile_linux_api::ProcessError> {
                unreachable!()
            }
            fn is_available(&self) -> bool {
                true
            }
            async fn terminate_and_wait_owned_background(
                &self,
                handle: &lingxi_core::host::ProcessHandle,
                _: &str,
            ) -> Result<(), mobile_linux_api::ProcessError> {
                self.started.send(handle.task_id.clone()).unwrap();
                if handle.task_id == "slow" {
                    self.release.notified().await;
                }
                Ok(())
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let runner = Arc::new(Runner {
            started: tx,
            release: release.clone(),
        });
        let request = Arc::new(HostToolRequest::default());
        for (id, pid) in [("slow", 1), ("fast", 2)] {
            request.track_background(
                runner.clone(),
                lingxi_core::host::ProcessHandle {
                    task_id: id.into(),
                    pid,
                },
                "owner".into(),
            );
        }
        assert!(request.cancel());
        let waiter = tokio::spawn({
            let request = request.clone();
            async move { request.cancel_and_reap().await }
        });
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first, second, "every owned handle must receive stop");
        waiter.abort();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), request.cancel_and_reap())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn host_retains_sandbox_lease_when_process_reap_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        struct RetryRunner {
            attempts: AtomicUsize,
            verified: Arc<tokio::sync::Notify>,
        }
        #[async_trait::async_trait]
        impl lingxi_core::host::ProcessRunner for RetryRunner {
            async fn run(
                &self,
                _: &lingxi_core::host::SandboxedCommand,
            ) -> Result<mobile_linux_api::ProcessOutput, mobile_linux_api::ProcessError>
            {
                unreachable!()
            }
            async fn spawn_background(
                &self,
                _: &lingxi_core::host::SandboxedCommand,
            ) -> Result<lingxi_core::host::ProcessHandle, mobile_linux_api::ProcessError>
            {
                unreachable!()
            }
            async fn kill(
                &self,
                _: &lingxi_core::host::ProcessHandle,
            ) -> Result<(), mobile_linux_api::ProcessError> {
                unreachable!()
            }
            fn is_available(&self) -> bool {
                true
            }
            async fn terminate_and_wait_owned_background(
                &self,
                _: &lingxi_core::host::ProcessHandle,
                _: &str,
            ) -> Result<(), mobile_linux_api::ProcessError> {
                if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(mobile_linux_api::ProcessError::Io(
                        "unverified cleanup".into(),
                    ));
                }
                self.verified.notified().await;
                Ok(())
            }
        }
        let releases = Arc::new(AtomicUsize::new(0));
        let verified = Arc::new(tokio::sync::Notify::new());
        let lease = crate::sandbox_runner::SandboxLease::new({
            let releases = releases.clone();
            move || {
                releases.fetch_add(1, Ordering::SeqCst);
            }
        });
        let request = Arc::new(HostToolRequest::default());
        request.track_background_with_cleanup(
            Arc::new(RetryRunner {
                attempts: AtomicUsize::new(0),
                verified: verified.clone(),
            }),
            lingxi_core::host::ProcessHandle {
                task_id: "unverified".into(),
                pid: 42,
            },
            "owner".into(),
            None,
            Some(lease),
        );
        assert!(request.cancel());
        assert!(request.cancel_and_reap().await.is_err());
        assert_eq!(
            releases.load(Ordering::SeqCst),
            0,
            "unverified reaping must retain resources"
        );
        drop(request);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            releases.load(Ordering::SeqCst),
            0,
            "dropping the request cannot release an unverified lease"
        );
        verified.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while releases.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(releases.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn host_cleanup_timeout_reports_unverified_pending_spawn() {
        use std::time::Duration;
        let request = Arc::new(HostToolRequest::default());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let launch = tokio::spawn({
            let request = request.clone();
            let entered = entered.clone();
            let release = release.clone();
            async move {
                request
                    .run_background(async move {
                        entered.notify_one();
                        release.notified().await;
                    })
                    .await
            }
        });
        entered.notified().await;
        assert!(request.handoff_task_ids().is_err());
        assert!(!request.commit());
        let error = request
            .cancel_and_reap_with_timeout(Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(error.contains("termination is unverified"), "{error}");
        release.notify_one();
        launch.await.unwrap().unwrap();
        assert!(
            request.cancel_and_reap().await.is_err(),
            "timeout evidence must persist"
        );
    }

    #[tokio::test]
    async fn host_background_commit_preserves_task_and_cleanup_failures_surface() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let stopped = Arc::new(AtomicUsize::new(0));
        let request = Arc::new(HostToolRequest::default());
        request.track_background(
            Arc::new(CleanupRunner {
                stopped: stopped.clone(),
                fail: false,
            }),
            lingxi_core::host::ProcessHandle {
                task_id: "committed".into(),
                pid: 1,
            },
            "host-tool:committed".into(),
        );
        assert!(
            request.handoff_task_ids().is_err(),
            "unacknowledged handles cannot be handed off"
        );
        assert!(!request.commit());
        assert!(
            request.mark_background_ready(&lingxi_core::host::ProcessHandle {
                task_id: "committed".into(),
                pid: 1,
            })
        );
        assert_eq!(request.handoff_task_ids().unwrap(), vec!["committed"]);
        assert!(request.commit());
        assert!(!request.cancel());
        request.cancel_and_reap().await.unwrap();
        assert_eq!(stopped.load(Ordering::SeqCst), 0);

        let failed = Arc::new(HostToolRequest::default());
        failed.track_background(
            Arc::new(CleanupRunner {
                stopped,
                fail: true,
            }),
            lingxi_core::host::ProcessHandle {
                task_id: "failed".into(),
                pid: 2,
            },
            "host-tool:failed".into(),
        );
        assert!(failed.cancel());
        assert!(failed
            .run_background(async { panic!("cancelled launch ran") })
            .await
            .unwrap()
            .is_none());
        assert!(failed
            .cancel_and_reap()
            .await
            .unwrap_err()
            .contains("unverified cleanup"));
    }

    #[tokio::test]
    async fn host_handoff_seals_admission_until_commit_or_cancel() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let stopped = Arc::new(AtomicUsize::new(0));
        let request = Arc::new(HostToolRequest::default());
        let handle = lingxi_core::host::ProcessHandle {
            task_id: "sealed".into(),
            pid: 42,
        };
        request.track_background(
            Arc::new(CleanupRunner {
                stopped: stopped.clone(),
                fail: false,
            }),
            handle.clone(),
            "host-tool:test".into(),
        );
        assert!(request.mark_background_ready(&handle));
        assert!(!request.commit(), "a response must first seal its task IDs");
        assert_eq!(request.handoff_task_ids().unwrap(), vec!["sealed"]);
        assert!(!request.is_active());
        assert!(request.lifecycle_token().is_cancelled());

        // A Mod task that was queued before the snapshot may arrive on a
        // different Tokio task after it. It must not spawn an extra process.
        let ran = Arc::new(AtomicBool::new(false));
        let attempted = tokio::spawn({
            let request = request.clone();
            let ran = ran.clone();
            async move {
                request
                    .run_background(async move { ran.store(true, Ordering::SeqCst) })
                    .await
                    .unwrap()
            }
        });
        assert!(attempted.await.unwrap().is_none());
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(request.handoff_task_ids().unwrap(), vec!["sealed"]);
        assert!(request.cancel(), "cancel must still win before flush");
        request.cancel_and_reap().await.unwrap();
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert!(!request.commit());

        let committed = Arc::new(HostToolRequest::default());
        assert!(committed.handoff_task_ids().unwrap().is_empty());
        assert!(committed.commit());
        assert!(!committed.cancel(), "flush and commit won first");
    }

    #[tokio::test]
    async fn host_sealed_handoff_outlives_request_until_delivery_decided() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let stopped = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let request = Arc::new(HostToolRequest::default());
        let handle = lingxi_core::host::ProcessHandle {
            task_id: "undecided".into(),
            pid: 17,
        };
        let lease = crate::sandbox_runner::SandboxLease::new({
            let released = released.clone();
            move || {
                released.fetch_add(1, Ordering::SeqCst);
            }
        });
        request.track_background_with_cleanup(
            Arc::new(CleanupRunner {
                stopped: stopped.clone(),
                fail: false,
            }),
            handle.clone(),
            "owner".into(),
            None,
            Some(lease),
        );
        assert!(request.mark_background_ready(&handle));
        assert_eq!(request.handoff_task_ids().unwrap(), vec!["undecided"]);
        let weak = Arc::downgrade(&request);
        drop(request);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert_eq!(stopped.load(Ordering::SeqCst), 0);
        let retained = weak
            .upgrade()
            .expect("undecided publication retains ownership");
        assert!(retained.cancel());
        retained.cancel_and_reap().await.unwrap();
        drop(retained);
        assert!(weak.upgrade().is_none());
        assert_eq!(stopped.load(Ordering::SeqCst), 1);
        assert_eq!(released.load(Ordering::SeqCst), 1);

        let request = Arc::new(HostToolRequest::default());
        assert!(request.handoff_task_ids().unwrap().is_empty());
        let weak = Arc::downgrade(&request);
        drop(request);
        let retained = weak.upgrade().expect("sealed request is still retained");
        assert!(retained.commit());
        drop(retained);
        assert!(
            weak.upgrade().is_none(),
            "a decided handoff must not leak its request"
        );
    }

    #[tokio::test]
    async fn host_mutation_admission_blocks_cleanup_and_sealing() {
        use std::time::Duration;
        let request = Arc::new(HostToolRequest::default());
        let lease = request
            .try_admit_mutation()
            .expect("active request admits mutation");
        assert!(
            request.handoff_task_ids().is_err(),
            "pending mutation blocks handoff"
        );
        assert!(request.cancel());
        assert!(
            request.try_admit_mutation().is_none(),
            "cancel closes admission"
        );
        let mut cleanup = tokio::spawn({
            let request = request.clone();
            async move { request.cancel_and_reap().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut cleanup)
                .await
                .is_err()
        );
        lease.complete();
        tokio::time::timeout(Duration::from_secs(2), cleanup)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let sealed = Arc::new(HostToolRequest::default());
        assert!(sealed.handoff_task_ids().unwrap().is_empty());
        assert!(
            sealed.try_admit_mutation().is_none(),
            "sealed handoff closes admission"
        );
        assert!(sealed.commit());

        let panicked = Arc::new(HostToolRequest::default());
        let lease = panicked.try_admit_mutation().unwrap();
        let task = tokio::spawn(async move {
            let _lease = lease;
            panic!("injected mutation failure");
        });
        assert!(task.await.is_err());
        let error = panicked.cancel_and_reap().await.unwrap_err();
        assert!(error.contains("mutation was interrupted"), "{error}");

        let aborted = Arc::new(HostToolRequest::default());
        let lease = aborted.try_admit_mutation().unwrap();
        let task = tokio::spawn(async move {
            let _lease = lease;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.is_err());
        let error = aborted.cancel_and_reap().await.unwrap_err();
        assert!(error.contains("mutation was interrupted"), "{error}");
    }
}

/// Per-call options that travel inside [`ToolUseContext`].
#[derive(Clone)]
pub struct ToolUseOptions {
    /// Emit debug-level traces from this call.
    pub debug: bool,
    /// Emit verbose informational traces from this call.
    pub verbose: bool,
    /// Identifier of the model driving the main agent loop.
    pub main_loop_model: String,
    /// Active provider profile for [`Self::main_loop_model`], when pinned
    /// (e.g. `github-copilot`). Tools that need provider-aware behavior should
    /// use this instead of unrelated host-specific request builders.
    pub model_profile: Option<String>,
    /// Optional cost budget for this call, in nano-USD.
    pub max_budget_nano_usd: Option<u64>,
    /// MCP server connections available for this call.
    pub mcp_clients: Vec<McpConnectionId>,
    /// Whether the agent is running in a non-interactive session.
    pub is_non_interactive_session: bool,
    /// Optional source-preserving system prompt override (replaces the default).
    pub custom_system_prompt: Option<Utf16JsonProjection>,
    /// Optional source-preserving system prompt addendum (appended to the default).
    pub append_system_prompt: Option<Utf16JsonProjection>,
}
