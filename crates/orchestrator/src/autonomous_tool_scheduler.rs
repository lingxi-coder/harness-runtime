use futures::FutureExt;
use futures::future::{BoxFuture, poll_fn};
use lingxi_core::host::CancellationToken;
use lingxi_core::host::tool_invoker::{ToolInvocationContextModifier, ToolInvocationContextState};
use lingxi_core::host::tool_use_lifecycle::{
    ToolUseLifecycleTracker, ToolUseRemoval, ToolUseRemovalReason,
};
use lingxi_core::types::{MessageId, ToolUseId};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tool_api::context::ToolUseContext;
use tool_api::tool_trait::Tool;

use crate::error::OrchestratorError;
use crate::turn_loop::{DeferredToolDispatch, ToolUseDispatchFacts};

pub(crate) type ModelContextResolver =
    Arc<dyn Fn(&ToolUseContext, ToolUseContext) -> Result<ToolUseContext, String> + Send + Sync>;

pub(crate) struct OwnedToolCall {
    pub(crate) id: ToolUseId,
    pub(crate) presented_name: String,
    pub(crate) canonical_name: String,
    pub(crate) input: serde_json::Value,
    pub(crate) provider_id: Option<String>,
    pub(crate) assistant_id: MessageId,
    pub(crate) facts: ToolUseDispatchFacts,
    pub(crate) resolved_tool: Arc<dyn Tool>,
    pub(crate) concurrency_safe: bool,
    pub(crate) cancellation: CancellationToken,
    /// Parent-generation fence. Unlike `cancellation`, this is not fired by a
    /// user interrupt; it becomes obsolete only when reset/drop discards the
    /// executor generation.
    pub(crate) publication_fence: Option<ToolDispatchPublicationFence>,
    /// Optional production barrier signalled at the real streaming dispatch entry.
    pub(crate) dispatch_started_tx: Option<oneshot::Sender<()>>,
    pub(crate) dispatch_started_rx: Option<oneshot::Receiver<()>>,
    #[cfg(test)]
    pub(crate) dispatch_finished_tx: Option<oneshot::Sender<()>>,
    pub(crate) tool_context_state: Option<ToolInvocationContextState>,
}

#[derive(Clone)]
pub(crate) struct ToolDispatchPublicationFence {
    generation_root: CancellationToken,
    publication_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ToolDispatchPublicationFence {
    pub(crate) fn new(
        generation_root: CancellationToken,
        publication_lock: Arc<tokio::sync::Mutex<()>>,
    ) -> Self {
        Self {
            generation_root,
            publication_lock,
        }
    }

    /// Serialize one external progress/UI publication against reset, while
    /// remaining cancellable if reset/drop happens during a backpressured sink.
    /// Result rows themselves are carried to Tn and committed only after actor
    /// acceptance; this lease is only for side-channel events.
    pub(crate) async fn publish_if_current(&self, publication: impl Future<Output = ()>) -> bool {
        let root = self.generation_root.clone();
        let lock = Arc::clone(&self.publication_lock);
        let _guard = tokio::select! {
            biased;
            () = root.cancelled() => return false,
            guard = lock.lock_owned() => guard,
        };
        if root.is_cancelled() {
            return false;
        }
        tokio::select! {
            biased;
            () = root.cancelled() => false,
            () = publication => true,
        }
    }

    pub(crate) fn is_current(&self) -> bool {
        !self.generation_root.is_cancelled()
    }

    pub(crate) fn generation_cancellation_token(&self) -> CancellationToken {
        self.generation_root.clone()
    }
}

impl hooks::attachment::HookPublicationGuard for ToolDispatchPublicationFence {
    fn is_current(&self) -> bool {
        ToolDispatchPublicationFence::is_current(self)
    }

    fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
        Some(self.generation_cancellation_token())
    }

    fn publish_if_current<'a>(
        &'a self,
        publication: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(ToolDispatchPublicationFence::publish_if_current(
            self,
            publication,
        ))
    }

    fn commit_if_current<'a>(
        &'a self,
        mutation: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        let root = self.generation_root.clone();
        let lock = Arc::clone(&self.publication_lock);
        Box::pin(async move {
            let _guard = tokio::select! {
                biased;
                () = root.cancelled() => return false,
                guard = lock.lock_owned() => guard,
            };
            if root.is_cancelled() {
                return false;
            }
            // Once admitted, finish even if reset cancels the root. Reset waits
            // for this lease, so blocking I/O cannot append after reset returns.
            mutation.await;
            true
        })
    }
}

pub(crate) struct ToolDispatchPayload {
    pub(crate) dispatch: DeferredToolDispatch,
    pub(crate) tool_context_state: Option<ToolInvocationContextState>,
    pub(crate) context_modifiers: Vec<ToolInvocationContextModifier>,
}

pub(crate) type ToolOutcome = Result<ToolDispatchPayload, OrchestratorError>;

pub(crate) type ToolDispatch =
    Arc<dyn Fn(OwnedToolCall) -> BoxFuture<'static, ToolOutcome> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct ReadyToolMeta {
    pub(crate) id: ToolUseId,
    pub(crate) presented_name: String,
    pub(crate) canonical_name: Option<String>,
    pub(crate) input: serde_json::Value,
    pub(crate) provider_id: Option<String>,
    pub(crate) assistant_id: MessageId,
    pub(crate) facts: Option<ToolUseDispatchFacts>,
    pub(crate) resolved_tool: Option<Arc<dyn Tool>>,
    pub(crate) concurrency_safe: bool,
    pub(crate) is_agent: bool,
}

impl From<&OwnedToolCall> for ReadyToolMeta {
    fn from(call: &OwnedToolCall) -> Self {
        Self {
            id: call.id.clone(),
            presented_name: call.presented_name.clone(),
            canonical_name: Some(call.canonical_name.clone()),
            input: call.input.clone(),
            provider_id: call.provider_id.clone(),
            assistant_id: call.assistant_id,
            facts: Some(call.facts.clone()),
            resolved_tool: Some(Arc::clone(&call.resolved_tool)),
            concurrency_safe: call.concurrency_safe,
            is_agent: call.canonical_name == "Agent",
        }
    }
}

pub(crate) struct CompletedDispatch {
    pub(crate) meta: ReadyToolMeta,
    pub(crate) context_state: Option<ToolInvocationContextState>,
    pub(crate) outcome: ToolOutcome,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Queued,
    Executing,
    Completed,
    Yielded,
}

struct Record {
    generation: u64,
    meta: ReadyToolMeta,
    queued_call: Option<OwnedToolCall>,
    status: Status,
    outcome: Option<ToolOutcome>,
    pending_context_modifiers: Vec<ToolInvocationContextModifier>,
    context_state: Option<ToolInvocationContextState>,
    call_cancellation: Option<CancellationToken>,
}

#[derive(Debug)]
pub(crate) enum SchedulerStopped {
    Unavailable,
    ModelResolution(String),
}

enum Command {
    Add(
        OwnedToolCall,
        oneshot::Sender<Result<bool, SchedulerStopped>>,
    ),
    AddReady(
        ReadyToolMeta,
        ToolOutcome,
        oneshot::Sender<Result<(), SchedulerStopped>>,
    ),
    TakeReady(oneshot::Sender<Result<Vec<CompletedDispatch>, SchedulerStopped>>),
    TakeAllReady(oneshot::Sender<Result<Vec<CompletedDispatch>, SchedulerStopped>>),
    IsIdle(oneshot::Sender<Result<bool, SchedulerStopped>>),
    Statuses(oneshot::Sender<Result<Vec<(ToolUseId, Status)>, SchedulerStopped>>),
    StopScheduling(oneshot::Sender<Result<(), SchedulerStopped>>),
    ApplyUserInterrupt(oneshot::Sender<Result<(), SchedulerStopped>>),
    Wait(oneshot::Sender<Result<(), SchedulerStopped>>),
    #[cfg(test)]
    WaitForCompletedCount(usize, oneshot::Sender<Result<(), SchedulerStopped>>),
    #[cfg(test)]
    WaitForObservedDispatchCount(usize, oneshot::Sender<Result<(), SchedulerStopped>>),
    FinishNormal(oneshot::Sender<Result<Option<ToolInvocationContextState>, SchedulerStopped>>),
    Reset(
        Option<ToolUseRemovalReason>,
        oneshot::Sender<Result<ToolUseRemoval, SchedulerStopped>>,
    ),
    Shutdown,
}

type RunningTask = Pin<Box<dyn Future<Output = (u64, usize, ToolOutcome)> + Send>>;

pub(crate) struct ToolScheduler {
    tx: mpsc::UnboundedSender<Command>,
    normal_finished: Arc<AtomicBool>,
    root_cancel_slot: Arc<Mutex<CancellationToken>>,
    reset_cancel_requested: Arc<AtomicBool>,
    stop_scheduling_requested: Arc<AtomicBool>,
    completion_commit_lock: Arc<Mutex<()>>,
    publication_commit_lock: Arc<tokio::sync::Mutex<()>>,
    lifecycle: ToolUseLifecycleTracker,
    #[cfg(test)]
    actor_terminated: Arc<tokio::sync::Notify>,
}

impl ToolScheduler {
    pub(crate) fn spawn(
        dispatch: ToolDispatch,
        max_safe_concurrency: usize,
        user_cancel: Option<CancellationToken>,
        initial_tool_context_state: Option<ToolInvocationContextState>,
        session_root: CancellationToken,
        publication_commit_lock: Arc<tokio::sync::Mutex<()>>,
        model_context_resolver: Option<ModelContextResolver>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        // User interruption is per-tool (InterruptBehavior::Cancel). The root
        // token belongs only to executor reset/discard and must not cancel Block tools.
        let root = session_root.child_token();
        let root_cancel_slot = Arc::new(Mutex::new(root.clone()));
        let reset_cancel_requested = Arc::new(AtomicBool::new(false));
        let stop_scheduling_requested = Arc::new(AtomicBool::new(false));
        let completion_commit_lock = Arc::new(Mutex::new(()));
        let normal_finished = Arc::new(AtomicBool::new(false));
        let actor = Actor {
            rx,
            dispatch,
            tasks: JoinSet::new(),
            records: Vec::new(),
            generation: 0,
            session_root,
            root,
            user_cancel,
            root_cancel_slot: Arc::clone(&root_cancel_slot),
            reset_cancel_requested: Arc::clone(&reset_cancel_requested),
            stop_scheduling_requested: Arc::clone(&stop_scheduling_requested),
            completion_commit_lock: Arc::clone(&completion_commit_lock),
            publication_commit_lock: Arc::clone(&publication_commit_lock),
            normal_finished: Arc::clone(&normal_finished),
            user_cancel_applied: false,
            max_safe_concurrency: max_safe_concurrency.max(1),
            base_tool_context_state: initial_tool_context_state.clone(),
            tool_context_state: initial_tool_context_state,
            model_context_changed: false,
            model_context_resolver,
            model_context_error: None,
            waiters: Vec::new(),
            #[cfg(test)]
            completed_count_waiters: Vec::new(),
            #[cfg(test)]
            observed_dispatch_count: 0,
            #[cfg(test)]
            observed_dispatch_waiters: Vec::new(),
        };
        #[cfg(test)]
        let actor_terminated = Arc::new(tokio::sync::Notify::new());
        #[cfg(test)]
        let actor_terminated_task = Arc::clone(&actor_terminated);
        tokio::spawn(lingxi_core::host::model_safety::bind_current_model_safety(
            async move {
                actor.run().await;
                #[cfg(test)]
                actor_terminated_task.notify_one();
            },
        ));
        Self {
            tx,
            normal_finished,
            root_cancel_slot,
            reset_cancel_requested,
            stop_scheduling_requested,
            completion_commit_lock,
            publication_commit_lock,
            lifecycle: ToolUseLifecycleTracker::default(),
            #[cfg(test)]
            actor_terminated,
        }
    }

    /// Capture the current actor generation for accepted rows that are not
    /// inside a `Tool::call` closure, including unknown-tool synthetics and
    /// assistant `session.append` rows. Serialize capture against fallback
    /// reset so the returned fence cannot bind the replacement root.
    pub(crate) fn current_publication_fence(&self) -> ToolDispatchPublicationFence {
        let _completion = self
            .completion_commit_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = self
            .root_cancel_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        ToolDispatchPublicationFence::new(root, Arc::clone(&self.publication_commit_lock))
    }

    pub(crate) async fn add(&mut self, call: OwnedToolCall) -> Result<bool, SchedulerStopped> {
        let id = call.id.clone();
        let is_agent = call.canonical_name == "Agent";
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Add(call, reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        let started = response
            .await
            .map_err(|_| SchedulerStopped::Unavailable)??;
        self.lifecycle.observe_assistant_row([(id, is_agent)]);
        Ok(started)
    }

    pub(crate) async fn add_ready(
        &mut self,
        meta: ReadyToolMeta,
        outcome: ToolOutcome,
    ) -> Result<(), SchedulerStopped> {
        let id = meta.id.clone();
        let is_agent = meta.is_agent;
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::AddReady(meta, outcome, reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response
            .await
            .map_err(|_| SchedulerStopped::Unavailable)??;
        self.lifecycle.observe_assistant_row([(id, is_agent)]);
        Ok(())
    }

    /// Exact Tn boundary: take already-completed entries, registration order,
    /// executing-unsafe barrier, no wait and no queue promotion.
    pub(crate) async fn take_ready(&mut self) -> Result<Vec<CompletedDispatch>, SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::TakeReady(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        let completed = response
            .await
            .map_err(|_| SchedulerStopped::Unavailable)??;
        for dispatch in &completed {
            self.lifecycle.observe_tool_result(&dispatch.meta.id);
        }
        Ok(completed)
    }

    /// Model-error Tn(false) second pass: ignore the unsafe barrier, still take
    /// only entries already complete, and do not start queued work.
    pub(crate) async fn take_all_ready(
        &mut self,
    ) -> Result<Vec<CompletedDispatch>, SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::TakeAllReady(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        let completed = response
            .await
            .map_err(|_| SchedulerStopped::Unavailable)??;
        for dispatch in &completed {
            self.lifecycle.observe_tool_result(&dispatch.meta.id);
        }
        Ok(completed)
    }

    pub(crate) async fn is_current_generation_idle(&self) -> Result<bool, SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::IsIdle(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    pub(crate) async fn statuses(&self) -> Result<Vec<(ToolUseId, Status)>, SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Statuses(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    /// Stop promoting queued calls as soon as the host enters a terminal
    /// provider/host-error drain. The atomic gate closes the completion race;
    /// the command acknowledges that the actor observed the transition.
    pub(crate) async fn stop_scheduling(&self) -> Result<(), SchedulerStopped> {
        self.stop_scheduling_requested
            .store(true, Ordering::Release);
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::StopScheduling(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    /// Reconcile the user interrupt into per-tool cancellation and queued
    /// synthetic completions before a host Tn scan.
    pub(crate) async fn apply_user_interrupt(&self) -> Result<(), SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::ApplyUserInterrupt(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    #[cfg(test)]
    async fn wait_for_completed_count(&self, count: usize) -> Result<(), SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::WaitForCompletedCount(count, reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    #[cfg(test)]
    pub(crate) fn actor_termination_notifier(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.actor_terminated)
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_observed_dispatch_completions(
        &self,
        count: usize,
    ) -> Result<(), SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::WaitForObservedDispatchCount(count, reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    pub(crate) async fn wait_for_progress(&self) -> Result<(), SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Wait(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    pub(crate) async fn finish_normal(
        &self,
    ) -> Result<Option<ToolInvocationContextState>, SchedulerStopped> {
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::FinishNormal(reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        response.await.map_err(|_| SchedulerStopped::Unavailable)?
    }

    pub(crate) async fn reset_after_server_fallback(
        &mut self,
        reason: Option<ToolUseRemovalReason>,
    ) -> Result<ToolUseRemoval, SchedulerStopped> {
        // Invalidate the generation before waiting for any external sink. A
        // progress/UI publication holds the shared lease only while its sink
        // future is alive; cancellation drops that future and releases the lease.
        // The actor then handles Reset and cannot promote old-generation work.
        self.normal_finished.store(false, Ordering::Release);
        let _commit = self
            .completion_commit_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reset_cancel_requested.store(true, Ordering::Release);
        self.root_cancel_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
        drop(_commit);
        let _publication = Arc::clone(&self.publication_commit_lock).lock_owned().await;
        let (reply, response) = oneshot::channel();
        self.tx
            .send(Command::Reset(reason, reply))
            .map_err(|_| SchedulerStopped::Unavailable)?;
        let removal = response
            .await
            .map_err(|_| SchedulerStopped::Unavailable)??;
        self.lifecycle.apply_removal(&removal);
        Ok(removal)
    }

    pub(crate) fn is_agent_idle(&self) -> bool {
        self.lifecycle.is_agent_idle()
    }
}

impl Drop for ToolScheduler {
    fn drop(&mut self) {
        if !self.normal_finished.load(Ordering::Acquire) {
            let _commit = self
                .completion_commit_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.reset_cancel_requested.store(true, Ordering::Release);
            self.root_cancel_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cancel();
            drop(_commit);
        }
        let _ = self.tx.send(Command::Shutdown);
    }
}

struct Actor {
    rx: mpsc::UnboundedReceiver<Command>,
    dispatch: ToolDispatch,
    tasks: JoinSet<(u64, usize, ToolOutcome)>,
    records: Vec<Record>,
    generation: u64,
    session_root: CancellationToken,
    root: CancellationToken,
    user_cancel: Option<CancellationToken>,
    root_cancel_slot: Arc<Mutex<CancellationToken>>,
    reset_cancel_requested: Arc<AtomicBool>,
    stop_scheduling_requested: Arc<AtomicBool>,
    completion_commit_lock: Arc<Mutex<()>>,
    publication_commit_lock: Arc<tokio::sync::Mutex<()>>,
    normal_finished: Arc<AtomicBool>,
    user_cancel_applied: bool,
    max_safe_concurrency: usize,
    /// Constructor-owned Native ToolUseContext shared by every rebuilt
    /// executor generation. Completed generation layers are discarded at
    /// reset; they must not erase or replace this immutable base.
    base_tool_context_state: Option<ToolInvocationContextState>,
    tool_context_state: Option<ToolInvocationContextState>,
    /// True when a modifier explicitly selected a model or provider. Even a
    /// canonical selection equal to the frozen context must be compared with
    /// live session state, which can change during a serving fallback. An
    /// unchanged modifier never projects the old snapshot back.
    model_context_changed: bool,
    model_context_resolver: Option<ModelContextResolver>,
    model_context_error: Option<String>,
    waiters: Vec<oneshot::Sender<Result<(), SchedulerStopped>>>,
    #[cfg(test)]
    completed_count_waiters: Vec<(usize, oneshot::Sender<Result<(), SchedulerStopped>>)>,
    #[cfg(test)]
    observed_dispatch_count: usize,
    #[cfg(test)]
    observed_dispatch_waiters: Vec<(usize, oneshot::Sender<Result<(), SchedulerStopped>>)>,
}

impl Actor {
    async fn run(mut self) {
        loop {
            let user_cancel = self.user_cancel.clone();
            tokio::select! {
                biased;
                joined = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    match joined {
                        Some(Ok((generation, index, outcome))) => {
                            if generation == self.generation {
                                if self.complete(index, generation, outcome).is_err() {
                                    self.root.cancel();
                                    self.fail_waiters();
                                    return;
                                }
                                self.apply_user_interrupts();
                                if self.schedule().await.is_err() {
                                    self.fail_waiters();
                                    return;
                                }
                                self.wake_waiters();
                            }
                            #[cfg(test)]
                            self.observe_dispatch_completion();
                        }
                        // Dispatch panics are converted inside the task. A
                        // JoinError is an actor/runtime failure and must not be
                        // reported as an empty ready result.
                        Some(Err(_)) => {
                            self.fail_waiters();
                            return;
                        }
                        None => {}
                    }
                }
                command = self.rx.recv() => {
                    let Some(command) = command else {
                        if !self.normal_finished.load(Ordering::Acquire) {
                            self.root.cancel();
                        }
                        return;
                    };
                    match command {
                        Command::Add(call, reply) => {
                            let meta = ReadyToolMeta::from(&call);
                            self.records.push(Record {
                                generation: self.generation,
                                context_state: call.tool_context_state.clone(),
                                meta,
                                queued_call: Some(call),
                                status: Status::Queued,
                                outcome: None,
                                pending_context_modifiers: Vec::new(),
                                call_cancellation: None,
                            });
                            self.apply_user_interrupts();
                            let result = self.schedule().await.map(|()| {
                                self.records.last().is_some_and(|record| {
                                    record.generation == self.generation
                                        && record.status == Status::Executing
                                })
                            });
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed {
                                self.root.cancel();
                                self.fail_waiters();
                                return;
                            }
                        }
                        Command::AddReady(meta, outcome, reply) => {
                            self.records.push(Record {
                                generation: self.generation,
                                context_state: None,
                                meta,
                                queued_call: None,
                                status: Status::Completed,
                                outcome: Some(outcome),
                                pending_context_modifiers: Vec::new(),
                                call_cancellation: None,
                            });
                            self.wake_waiters();
                            let _ = reply.send(Ok(()));
                        }
                        Command::TakeReady(reply) => {
                            let _ = reply.send(Ok(self.take_ready(false)));
                        }
                        Command::TakeAllReady(reply) => {
                            let _ = reply.send(Ok(self.take_ready(true)));
                        }
                        Command::IsIdle(reply) => {
                            let _ = reply.send(Ok(self.is_idle()));
                        }
                        Command::Statuses(reply) => {
                            let statuses = self
                                .records
                                .iter()
                                .filter(|record| record.generation == self.generation)
                                .map(|record| (record.meta.id.clone(), record.status))
                                .collect();
                            let _ = reply.send(Ok(statuses));
                        }
                        Command::StopScheduling(reply) => {
                            let _ = reply.send(Ok(()));
                        }
                        Command::ApplyUserInterrupt(reply) => {
                            self.apply_user_interrupts();
                            let result = self.schedule().await;
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed {
                                self.root.cancel();
                                self.fail_waiters();
                                return;
                            }
                            self.wake_waiters();
                        }
                        Command::Wait(reply) => {
                            if self.has_tn_ready() || self.is_idle() {
                                let _ = reply.send(Ok(()));
                            } else {
                                self.waiters.push(reply);
                            }
                        }
                        #[cfg(test)]
                        Command::WaitForCompletedCount(count, reply) => {
                            if self.completed_count() >= count {
                                let _ = reply.send(Ok(()));
                            } else {
                                self.completed_count_waiters.push((count, reply));
                            }
                        }
                        #[cfg(test)]
                        Command::WaitForObservedDispatchCount(count, reply) => {
                            if self.observed_dispatch_count >= count {
                                let _ = reply.send(Ok(()));
                            } else {
                                self.observed_dispatch_waiters.push((count, reply));
                            }
                        }
                        Command::FinishNormal(reply) => {
                            let result = if self.is_idle() {
                                self.apply_safe_context_layers().and_then(|()| {
                                    if let Some(reason) = self.model_context_error.as_ref() {
                                        return Err(SchedulerStopped::ModelResolution(reason.clone()));
                                    }
                                    Ok(self.model_context_changed
                                        .then(|| self.tool_context_state.clone())
                                        .flatten())
                                })
                            } else {
                                Err(SchedulerStopped::Unavailable)
                            };
                            if result.is_ok() {
                                self.normal_finished.store(true, Ordering::Release);
                            }
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed {
                                self.root.cancel();
                                self.fail_waiters();
                                return;
                            }
                        }
                        Command::Reset(reason, reply) => {
                            let _ = reply.send(Ok(self.reset(reason)));
                        }
                        Command::Shutdown => {
                            if !self.normal_finished.load(Ordering::Acquire) {
                                self.root.cancel();
                            }
                            return;
                        }
                    }
                }
                _ = async {
                    if let Some(token) = user_cancel.as_ref() {
                        token.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if !self.user_cancel_applied && user_cancel.is_some() => {
                    self.user_cancel_applied = true;
                    self.apply_user_interrupts();
                    if self.schedule().await.is_err() {
                        self.root.cancel();
                        self.fail_waiters();
                        return;
                    }
                    self.wake_waiters();
                }
            }
        }
    }

    async fn schedule(&mut self) -> Result<(), SchedulerStopped> {
        if self.root.is_cancelled() || self.stop_scheduling_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        self.apply_user_interrupts();
        loop {
            if self.root.is_cancelled() || self.stop_scheduling_requested.load(Ordering::Acquire) {
                return Ok(());
            }
            let executing = self
                .records
                .iter()
                .filter(|record| {
                    record.generation == self.generation && record.status == Status::Executing
                })
                .map(|record| record.meta.concurrency_safe)
                .collect::<Vec<_>>();
            let safe_count = executing.iter().filter(|safe| **safe).count();
            let mut started = false;
            for index in 0..self.records.len() {
                let record = &self.records[index];
                if record.generation != self.generation || record.status != Status::Queued {
                    continue;
                }
                let safe = record.meta.concurrency_safe;
                if safe && safe_count >= self.max_safe_concurrency {
                    continue;
                }
                let can_execute =
                    executing.is_empty() || (safe && executing.iter().all(|active| *active));
                if can_execute {
                    // Native processQueue applies ended-run layers immediately
                    // before starting the next eligible call; the final drain
                    // is the other application boundary when no suffix exists.
                    self.apply_safe_context_layers()?;
                    self.apply_user_interrupts();
                    started = self.start(index).await?;
                    if !started {
                        if self.root.is_cancelled()
                            || self.stop_scheduling_requested.load(Ordering::Acquire)
                        {
                            return Ok(());
                        }
                        continue;
                    }
                    break;
                }
                if !safe {
                    return Ok(());
                }
            }
            if !started {
                return Ok(());
            }
        }
    }

    async fn start(&mut self, index: usize) -> Result<bool, SchedulerStopped> {
        if self.root.is_cancelled() || self.stop_scheduling_requested.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.apply_user_interrupts();
        let generation = self.generation;
        let Some(record) = self.records.get_mut(index) else {
            return Err(SchedulerStopped::Unavailable);
        };
        if record.generation != generation || record.status != Status::Queued {
            return Ok(false);
        }
        let Some(mut call) = record.queued_call.take() else {
            return Err(SchedulerStopped::Unavailable);
        };
        if record.status != Status::Queued {
            return Ok(false);
        }
        record.status = Status::Executing;
        call.cancellation = self.root.child_token();
        let call_cancellation = call.cancellation.clone();
        call.tool_context_state = self.tool_context_state.clone();
        let dispatch_started_wait = call.dispatch_started_rx.take();
        #[cfg(test)]
        let dispatch_finished_tx = call.dispatch_finished_tx.take();
        record.call_cancellation = Some(call_cancellation);
        call.publication_fence = Some(ToolDispatchPublicationFence::new(
            self.root.clone(),
            Arc::clone(&self.publication_commit_lock),
        ));
        let (started_tx, started_rx) = oneshot::channel();
        let dispatch = Arc::clone(&self.dispatch);
        self.tasks
            .spawn(lingxi_core::host::model_safety::bind_current_model_safety(
                async move {
                    let dispatch_future = async move { dispatch(call).await };
                    let mut dispatch_future =
                        Box::pin(std::panic::AssertUnwindSafe(dispatch_future).catch_unwind());
                    let mut started_tx = Some(started_tx);
                    let caught = poll_fn(|cx| {
                        let polled = dispatch_future.as_mut().poll(cx);
                        if let Some(started_tx) = started_tx.take() {
                            let _ = started_tx.send(());
                        }
                        polled
                    })
                    .await;
                    let outcome = match caught {
                        Ok(outcome) => outcome,
                        Err(_panic) => Err(OrchestratorError::StreamingProtocol(
                            "owned streaming tool dispatch panicked".into(),
                        )),
                    };
                    #[cfg(test)]
                    if let Some(finished) = dispatch_finished_tx {
                        let _ = finished.send(());
                    }
                    (generation, index, outcome)
                },
            ));
        // Spawn alone does not guarantee the callback was polled. This waits
        // only to first poll, never through permission or tool completion.
        started_rx
            .await
            .map_err(|_| SchedulerStopped::Unavailable)?;
        if let Some(dispatch_started_wait) = dispatch_started_wait {
            dispatch_started_wait
                .await
                .map_err(|_| SchedulerStopped::Unavailable)?;
        }
        Ok(true)
    }

    fn complete(
        &mut self,
        index: usize,
        generation: u64,
        mut outcome: ToolOutcome,
    ) -> Result<(), SchedulerStopped> {
        // Serialize result/context publication with reset's synchronous fence.
        // If reset wins the lock, a just-joined old result cannot leak a layer;
        // if completion wins, Native ordering accepts it before the fallback.
        let _commit = self
            .completion_commit_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.reset_cancel_requested.load(Ordering::Acquire) || generation != self.generation {
            return Ok(());
        }
        let Some(record) = self.records.get(index) else {
            return Ok(());
        };
        if record.generation != generation || record.status != Status::Executing {
            return Ok(());
        }
        let concurrency_safe = record.meta.concurrency_safe;
        let user_cancelled = self
            .user_cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled);
        let interrupt_behavior = record
            .meta
            .resolved_tool
            .as_ref()
            .map(|tool| tool.interrupt_behavior(&record.meta.input));
        let user_abort_wins = user_cancelled
            && matches!(
                interrupt_behavior,
                Some(tool_api::tool_trait::InterruptBehavior::Cancel)
            );
        let recorded_context_state = record.context_state.clone();
        let mut pending_safe_modifiers = Vec::new();
        let mut context_failure = None;
        if user_abort_wins {
            outcome = Err(OrchestratorError::StreamingProtocol(
                "tool interrupted before its result was accepted".into(),
            ));
        }
        if let Ok(outcome) = outcome.as_mut() {
            let returned_context_state =
                outcome.tool_context_state.take().or(recorded_context_state);
            if self.tool_context_state.is_none() {
                self.tool_context_state = returned_context_state.clone();
            }
            if concurrency_safe {
                pending_safe_modifiers = std::mem::take(&mut outcome.context_modifiers);
            } else {
                if let Some(state) = returned_context_state {
                    self.tool_context_state = Some(state);
                }
                let previous_state = self.tool_context_state.clone();
                let previously_changed = self.model_context_changed;
                for modifier in std::mem::take(&mut outcome.context_modifiers) {
                    let state = self
                        .tool_context_state
                        .as_ref()
                        .ok_or(SchedulerStopped::Unavailable)?;
                    let result = match self.model_context_error.as_ref() {
                        Some(reason) => Err(SchedulerStopped::ModelResolution(reason.clone())),
                        None => apply_context_modifier_and_detect_model_change(
                            state,
                            modifier,
                            self.model_context_resolver.as_ref(),
                        ),
                    };
                    match result {
                        Ok((updated, model_changed)) => {
                            self.tool_context_state = Some(updated);
                            self.model_context_changed |= model_changed;
                        }
                        Err(SchedulerStopped::ModelResolution(reason)) => {
                            self.tool_context_state = previous_state;
                            self.model_context_changed = previously_changed;
                            self.model_context_error = Some(reason.clone());
                            context_failure = Some(reason);
                            break;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        if let Some(reason) = context_failure {
            outcome = Err(OrchestratorError::StreamingProtocol(format!(
                "tool model preference could not be resolved: {reason}"
            )));
        }
        let current_state = self.tool_context_state.clone();
        if let Some(record) = self.records.get_mut(index) {
            record.pending_context_modifiers = pending_safe_modifiers;
            record.context_state = current_state.clone();
            record.outcome = Some(outcome);
            record.status = Status::Completed;
            record.call_cancellation = None;
        }
        if !concurrency_safe {
            if let Some(state) = current_state {
                for queued in self.records.iter_mut().filter(|queued| {
                    queued.generation == generation && queued.status == Status::Queued
                }) {
                    if let Some(call) = queued.queued_call.as_mut() {
                        call.tool_context_state = Some(state.clone());
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_user_interrupts(&mut self) {
        if !self
            .user_cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return;
        }
        let generation = self.generation;
        for record in self.records.iter_mut().filter(|record| {
            record.generation == generation
                && record.meta.resolved_tool.as_ref().is_some_and(|tool| {
                    matches!(
                        tool.interrupt_behavior(&record.meta.input),
                        tool_api::tool_trait::InterruptBehavior::Cancel
                    )
                })
        }) {
            match record.status {
                Status::Queued => {
                    record.queued_call = None;
                    record.status = Status::Completed;
                    record.outcome = Some(Err(OrchestratorError::StreamingProtocol(
                        "tool interrupted before execution".into(),
                    )));
                    record.context_state = self.tool_context_state.clone();
                }
                Status::Executing => {
                    if let Some(cancellation) = record.call_cancellation.as_ref() {
                        cancellation.cancel();
                    }
                }
                Status::Completed | Status::Yielded => {}
            }
        }
    }

    fn apply_safe_context_layers(&mut self) -> Result<(), SchedulerStopped> {
        if self.records.iter().any(|record| {
            record.generation == self.generation && record.status == Status::Executing
        }) {
            return Ok(());
        }
        if self.model_context_error.is_some() {
            for record in &mut self.records {
                record.pending_context_modifiers.clear();
            }
            return Ok(());
        }
        let previous_state = self.tool_context_state.clone();
        let previously_changed = self.model_context_changed;
        // Apply in registration order only after every executing tool settled.
        for index in 0..self.records.len() {
            let modifiers = {
                let record = &mut self.records[index];
                if record.generation == self.generation && record.meta.concurrency_safe {
                    std::mem::take(&mut record.pending_context_modifiers)
                } else {
                    Vec::new()
                }
            };
            for modifier in modifiers {
                let state = self
                    .tool_context_state
                    .as_ref()
                    .ok_or(SchedulerStopped::Unavailable)?;
                match apply_context_modifier_and_detect_model_change(
                    state,
                    modifier,
                    self.model_context_resolver.as_ref(),
                ) {
                    Ok((updated, model_changed)) => {
                        self.tool_context_state = Some(updated);
                        self.model_context_changed |= model_changed;
                        self.records[index].context_state = self.tool_context_state.clone();
                    }
                    Err(SchedulerStopped::ModelResolution(reason)) => {
                        self.tool_context_state = previous_state;
                        self.model_context_changed = previously_changed;
                        self.model_context_error = Some(reason);
                        for record in &mut self.records {
                            record.pending_context_modifiers.clear();
                            if record.generation == self.generation {
                                record.context_state = self.tool_context_state.clone();
                            }
                        }
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    fn take_ready(&mut self, ignore_unsafe_barrier: bool) -> Vec<CompletedDispatch> {
        let generation = self.generation;
        let mut out = Vec::new();
        for record in &mut self.records {
            if record.generation != generation {
                continue;
            }
            match record.status {
                Status::Completed => {
                    record.status = Status::Yielded;
                    if let Some(outcome) = record.outcome.take() {
                        out.push(CompletedDispatch {
                            meta: record.meta.clone(),
                            context_state: record.context_state.clone(),
                            outcome,
                        });
                    }
                }
                Status::Yielded | Status::Queued => {}
                Status::Executing if !ignore_unsafe_barrier && !record.meta.concurrency_safe => {
                    break;
                }
                Status::Executing => {}
            }
        }
        out
    }

    fn has_tn_ready(&self) -> bool {
        for record in &self.records {
            if record.generation != self.generation {
                continue;
            }
            match record.status {
                Status::Completed => return true,
                Status::Executing if !record.meta.concurrency_safe => return false,
                Status::Queued | Status::Yielded | Status::Executing => {}
            }
        }
        false
    }

    fn is_idle(&self) -> bool {
        self.records
            .iter()
            .all(|record| record.generation != self.generation || record.status == Status::Yielded)
    }

    fn wake_waiters(&mut self) {
        if self.has_tn_ready() || self.is_idle() {
            for waiter in self.waiters.drain(..) {
                let _ = waiter.send(Ok(()));
            }
        }
        #[cfg(test)]
        {
            let completed = self.completed_count();
            let mut pending = Vec::new();
            for (count, waiter) in self.completed_count_waiters.drain(..) {
                if completed >= count {
                    let _ = waiter.send(Ok(()));
                } else {
                    pending.push((count, waiter));
                }
            }
            self.completed_count_waiters = pending;
        }
    }

    #[cfg(test)]
    fn completed_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| {
                record.generation == self.generation && record.status == Status::Completed
            })
            .count()
    }

    #[cfg(test)]
    fn observe_dispatch_completion(&mut self) {
        self.observed_dispatch_count += 1;
        let mut pending = Vec::new();
        for (count, waiter) in self.observed_dispatch_waiters.drain(..) {
            if self.observed_dispatch_count >= count {
                let _ = waiter.send(Ok(()));
            } else {
                pending.push((count, waiter));
            }
        }
        self.observed_dispatch_waiters = pending;
    }

    fn fail_waiters(&mut self) {
        for waiter in self.waiters.drain(..) {
            let _ = waiter.send(Err(SchedulerStopped::Unavailable));
        }
    }

    fn reset(&mut self, reason: Option<ToolUseRemovalReason>) -> ToolUseRemoval {
        let old_generation = self.generation;
        let ids = self
            .records
            .iter()
            .filter(|record| record.generation == old_generation)
            .map(|record| record.meta.id.clone())
            .collect();
        self.root.cancel();
        self.records
            .retain(|record| record.generation != old_generation);
        self.generation = self.generation.saturating_add(1);
        self.reset_cancel_requested.store(false, Ordering::Release);
        self.stop_scheduling_requested
            .store(false, Ordering::Release);
        self.user_cancel_applied = false;
        self.normal_finished.store(false, Ordering::Release);
        self.tool_context_state = self.base_tool_context_state.clone();
        self.model_context_changed = false;
        self.model_context_error = None;
        self.root = self.session_root.child_token();
        *self
            .root_cancel_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.root.clone();
        let removal = ToolUseRemoval { ids, reason };
        self.wake_waiters();
        #[cfg(test)]
        for (_, waiter) in self.completed_count_waiters.drain(..) {
            let _ = waiter.send(Err(SchedulerStopped::Unavailable));
        }
        removal
    }
}

impl Drop for Actor {
    fn drop(&mut self) {
        // Native discard aborts the per-call controller but does not cancel a
        // non-cooperative promise. Dropping a Tokio JoinSet aborts every task,
        // so detach them before the actor's JoinSet is dropped. Their captured
        // cancellation tokens and generation remain call-local; with no actor
        // left to join them, late outcomes cannot update context or readiness.
        if !self.normal_finished.load(Ordering::Acquire) {
            self.root.cancel();
        }
        self.tasks.detach_all();
    }
}

fn apply_context_modifier(
    state: &ToolInvocationContextState,
    modifier: ToolInvocationContextModifier,
) -> Result<ToolInvocationContextState, SchedulerStopped> {
    let context = state
        .downcast_arc::<ToolUseContext>()
        .map_err(|_| SchedulerStopped::Unavailable)?;
    let updated = modifier
        .apply::<ToolUseContext>(context.as_ref().clone())
        .map_err(|_| SchedulerStopped::Unavailable)?;
    Ok(ToolInvocationContextState::new(Arc::new(updated)))
}

fn apply_context_modifier_and_detect_model_change(
    state: &ToolInvocationContextState,
    modifier: ToolInvocationContextModifier,
    resolver: Option<&ModelContextResolver>,
) -> Result<(ToolInvocationContextState, bool), SchedulerStopped> {
    let previous = state
        .downcast_arc::<ToolUseContext>()
        .map_err(|_| SchedulerStopped::Unavailable)?;
    let updated = apply_context_modifier(state, modifier)?;
    let updated = updated
        .downcast_arc::<ToolUseContext>()
        .map_err(|_| SchedulerStopped::Unavailable)?;
    let raw_changed = previous.options.main_loop_model != updated.options.main_loop_model
        || previous.options.model_profile != updated.options.model_profile;
    if !raw_changed {
        return Ok((ToolInvocationContextState::new(updated), false));
    }
    let updated = match resolver {
        Some(resolve) => resolve(previous.as_ref(), updated.as_ref().clone()).map_err(|error| {
            tracing::warn!(%error, "tool model preference rejected before context publication");
            SchedulerStopped::ModelResolution(error)
        })?,
        None => updated.as_ref().clone(),
    };
    Ok((
        ToolInvocationContextState::new(Arc::new(updated)),
        raw_changed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::types::{ContentBlock, ConversationMessage};
    use permission::{PermissionDecisionReason, PermissionResult, result::PermissionMetadata};
    use serde_json::json;
    use std::collections::HashMap;
    use tool_api::progress::ToolProgressSender;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, ToolCallResult, ToolError, ToolStaticContext,
        ValidationError,
    };

    struct ProbeTool {
        interrupt_behavior: tool_api::tool_trait::InterruptBehavior,
    }

    #[async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            "Probe"
        }

        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: std::sync::LazyLock<serde_json::Value> =
                std::sync::LazyLock::new(|| json!({"type":"object"}));
            &SCHEMA
        }

        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }

        fn max_result_size_chars(&self) -> usize {
            4096
        }

        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            false
        }

        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }

        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }

        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> PermissionResult {
            PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "actor test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            }
        }

        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "probe".into()
        }

        async fn prompt(&self, _: &PromptOptions) -> String {
            String::new()
        }

        fn interrupt_behavior(
            &self,
            _: &serde_json::Value,
        ) -> tool_api::tool_trait::InterruptBehavior {
            self.interrupt_behavior
        }

        async fn call(
            &self,
            _: serde_json::Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult::from_data(json!({"ok":true})))
        }
    }

    fn owned_call(id: &str) -> OwnedToolCall {
        let id = ToolUseId::from(id);
        let assistant_id = MessageId::new();
        OwnedToolCall {
            id: id.clone(),
            presented_name: "Probe".into(),
            canonical_name: "Probe".into(),
            input: json!({}),
            provider_id: None,
            assistant_id,
            facts: ToolUseDispatchFacts {
                query_history: Vec::new(),
                assistant_message: ConversationMessage::Assistant { per_turn_effort: None,
                    id: assistant_id,
                    content: vec![ContentBlock::ToolUse {
                        input_projection: None,
                        id,
                        name: "Probe".into(),
                        input: json!({}),
                        provider_id: None,
                    }],
                    stop_reason: None,
                },
                same_turn_tool_uses: Vec::new(),
            },
            resolved_tool: Arc::new(ProbeTool {
                interrupt_behavior: tool_api::tool_trait::InterruptBehavior::Block,
            }),
            concurrency_safe: false,
            cancellation: CancellationToken::new(),
            publication_fence: None,
            dispatch_started_tx: None,
            dispatch_started_rx: None,
            #[cfg(test)]
            dispatch_finished_tx: None,
            tool_context_state: None,
        }
    }

    fn result(id: &ToolUseId) -> ToolOutcome {
        Ok(ToolDispatchPayload {
            dispatch: DeferredToolDispatch {
                results: vec![ContentBlock::ToolResult {
                    content_projection: None,
                    tool_use_id: id.clone(),
                    content: id.as_str().to_owned(),
                    is_error: Some(false),
                    provider_tool_use_id: None,
                    content_blocks: None,
                }],
                prevent_continuation: false,
                injected_messages: Vec::new(),
                context_modifiers: Vec::new(),
                post_tool_batch_calls: Vec::new(),
                publications: Vec::new(),
            },
            tool_context_state: None,
            context_modifiers: Vec::new(),
        })
    }

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(2), future)
            .await
            .expect("actor stage must make progress")
    }

    fn test_session_generation() -> (CancellationToken, Arc<tokio::sync::Mutex<()>>) {
        (
            CancellationToken::new(),
            Arc::new(tokio::sync::Mutex::new(())),
        )
    }

    #[tokio::test]
    async fn normal_finish_preserves_generation_until_session_reset() {
        let session_root = CancellationToken::new();
        let scheduler_session_root = session_root.clone();
        let publication_lock = Arc::new(tokio::sync::Mutex::new(()));
        let captured_root = Arc::new(Mutex::new(None));
        let dispatch_root = Arc::clone(&captured_root);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let id = call.id.clone();
            *dispatch_root
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
                call.publication_fence
                    .as_ref()
                    .expect("scheduler installs generation fence")
                    .generation_cancellation_token(),
            );
            Box::pin(async move { result(&id) })
        });
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            None,
            scheduler_session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(owned_call("toolu-normal-close")))
            .await
            .unwrap();
        bounded(scheduler.wait_for_progress()).await.unwrap();
        assert_eq!(bounded(scheduler.take_ready()).await.unwrap().len(), 1);
        bounded(scheduler.finish_normal()).await.unwrap();
        let generation_root = captured_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .expect("dispatch captured its generation root");

        drop(scheduler);
        assert!(
            !generation_root.is_cancelled(),
            "normal executor teardown must keep accepted async hooks live"
        );

        session_root.cancel();
        assert!(
            generation_root.is_cancelled(),
            "clear/hot-resume retires generations from the mounted session"
        );
        let new_root = CancellationToken::new();
        assert!(
            !new_root.is_cancelled(),
            "the replacement session starts with a fresh owner"
        );
    }

    #[tokio::test]
    async fn add_starts_dispatch_without_a_provider_or_tn_poll() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(Mutex::new(
            HashMap::<ToolUseId, oneshot::Receiver<()>>::new(),
        ));
        let mut release_senders = HashMap::new();
        let id = ToolUseId::from("toolu-autonomous");
        let (release_tx, release_rx) = oneshot::channel();
        release.lock().unwrap().insert(id.clone(), release_rx);
        release_senders.insert(id.clone(), release_tx);

        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                let call_id = call.id.clone();
                let _ = started.send((call_id.clone(), call.cancellation.clone()));
                let receiver = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&call_id)
                    .expect("test release exists");
                let _ = receiver.await;
                result(&call_id)
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            None,
            session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(owned_call(id.as_str())))
            .await
            .unwrap();

        let (started_id, cancellation) = started_rx
            .try_recv()
            .expect("Add acknowledgment follows the dispatch first poll");
        assert_eq!(started_id, id);
        assert!(!cancellation.is_cancelled());

        // No provider event / take_ready call was needed to reach dispatch.
        let _ = release_senders.remove(&id).unwrap().send(());
        bounded(scheduler.wait_for_progress()).await.unwrap();
        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].meta.id, id);
    }

    #[tokio::test]
    async fn fallback_cancels_started_call_before_clearing_queued_suffix() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(
            HashMap::<ToolUseId, oneshot::Receiver<()>>::new(),
        ));
        let mut release_senders = HashMap::new();
        let first = ToolUseId::from("toolu-first");
        let second = ToolUseId::from("toolu-queued");
        for id in [&first, &second] {
            let (tx, rx) = oneshot::channel();
            release.lock().unwrap().insert(id.clone(), rx);
            release_senders.insert(id.clone(), tx);
        }

        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                let id = call.id.clone();
                let token = call.cancellation.clone();
                let _ = started.send(id.clone());
                let receiver = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                    .expect("test release exists");
                tokio::select! {
                    _ = token.cancelled() => {}
                    _ = receiver => {}
                }
                result(&id)
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            None,
            session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(owned_call(first.as_str())))
            .await
            .unwrap();
        assert_eq!(bounded(started_rx.recv()).await.as_ref(), Some(&first));
        bounded(scheduler.add(owned_call(second.as_str())))
            .await
            .unwrap();
        assert!(started_rx.try_recv().is_err(), "unsafe suffix stays queued");

        let removal = bounded(
            scheduler.reset_after_server_fallback(Some(ToolUseRemovalReason::FallbackSweep)),
        )
        .await
        .unwrap();
        assert_eq!(removal.ids, vec![first.clone(), second.clone()]);
        assert_eq!(removal.reason, Some(ToolUseRemovalReason::FallbackSweep));

        // Completing the cancelled first call cannot promote the queued second
        // call or leak a stale-generation result into Tn.
        tokio::task::yield_now().await;
        assert!(started_rx.try_recv().is_err());
        assert!(
            bounded(scheduler.is_current_generation_idle())
                .await
                .unwrap()
        );
        assert!(bounded(scheduler.take_ready()).await.unwrap().is_empty());
        drop(release_senders);
    }

    fn call_with_safety(id: &str, concurrency_safe: bool) -> OwnedToolCall {
        let mut call = owned_call(id);
        call.concurrency_safe = concurrency_safe;
        call
    }

    fn cancel_call_with_safety(id: &str, concurrency_safe: bool) -> OwnedToolCall {
        let mut call = call_with_safety(id, concurrency_safe);
        call.resolved_tool = Arc::new(ProbeTool {
            interrupt_behavior: tool_api::tool_trait::InterruptBehavior::Cancel,
        });
        call
    }

    fn seed_context_state() -> ToolInvocationContextState {
        ToolInvocationContextState::new(Arc::new(ToolUseContext::model_seed("seed".into(), None)))
    }

    fn context_model(state: &ToolInvocationContextState) -> String {
        state
            .downcast_arc::<ToolUseContext>()
            .expect("scheduler preserves ToolUseContext")
            .options
            .main_loop_model
            .clone()
    }

    fn payload_with_modifiers(
        id: &ToolUseId,
        context_modifiers: Vec<ToolInvocationContextModifier>,
    ) -> ToolOutcome {
        let mut payload = match result(id) {
            Ok(payload) => payload,
            Err(error) => return Err(error),
        };
        payload.context_modifiers = context_modifiers;
        Ok(payload)
    }

    #[tokio::test]
    async fn completed_unsafe_call_starts_queued_successor_without_a_poll() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(
            HashMap::<ToolUseId, oneshot::Receiver<()>>::new(),
        ));
        let mut release_senders = HashMap::new();
        let first = ToolUseId::from("toolu-unsafe-a");
        let second = ToolUseId::from("toolu-unsafe-b");
        let (release_tx, release_rx) = oneshot::channel();
        release.lock().unwrap().insert(first.clone(), release_rx);
        release_senders.insert(first.clone(), release_tx);

        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send(id.clone());
                let receiver = {
                    release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id)
                };
                if let Some(receiver) = receiver {
                    let _ = receiver.await;
                }
                result(&id)
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            2,
            None,
            None,
            session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(call_with_safety(first.as_str(), false)))
            .await
            .unwrap();
        assert_eq!(bounded(started_rx.recv()).await, Some(first.clone()));
        bounded(scheduler.add(call_with_safety(second.as_str(), false)))
            .await
            .unwrap();
        assert!(started_rx.try_recv().is_err(), "unsafe successor is queued");

        let _ = release_senders.remove(&first).unwrap().send(());
        assert_eq!(bounded(started_rx.recv()).await, Some(second));
        // No TakeReady/Tn request was made between A's completion and B's W1.
    }

    #[tokio::test]
    async fn t_n_returns_completed_safe_calls_in_registration_order() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(
            HashMap::<ToolUseId, oneshot::Receiver<()>>::new(),
        ));
        let mut release_senders = HashMap::new();
        let first = ToolUseId::from("toolu-safe-a");
        let second = ToolUseId::from("toolu-safe-b");
        for id in [&first, &second] {
            let (tx, rx) = oneshot::channel();
            release.lock().unwrap().insert(id.clone(), rx);
            release_senders.insert(id.clone(), tx);
        }
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send(id.clone());
                let receiver = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                    .expect("release exists");
                let _ = receiver.await;
                result(&id)
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            2,
            None,
            None,
            session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(call_with_safety(first.as_str(), true)))
            .await
            .unwrap();
        bounded(scheduler.add(call_with_safety(second.as_str(), true)))
            .await
            .unwrap();
        let mut started = vec![
            bounded(started_rx.recv()).await.unwrap(),
            bounded(started_rx.recv()).await.unwrap(),
        ];
        started.sort_by_key(|id| if id == &first { 0 } else { 1 });
        assert_eq!(started, vec![first.clone(), second.clone()]);

        let _ = release_senders.remove(&second).unwrap().send(());
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        let _ = release_senders.remove(&first).unwrap().send(());
        bounded(scheduler.wait_for_completed_count(2))
            .await
            .unwrap();
        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(
            ready
                .iter()
                .map(|entry| entry.meta.id.clone())
                .collect::<Vec<_>>(),
            vec![first, second],
            "completion order must not replace registration order"
        );
    }

    #[tokio::test]
    async fn t_n_can_take_later_safe_result_while_earlier_safe_call_runs() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(
            HashMap::<ToolUseId, oneshot::Receiver<()>>::new(),
        ));
        let mut release_senders = HashMap::new();
        let first = ToolUseId::from("toolu-running-a");
        let second = ToolUseId::from("toolu-ready-b");
        for id in [&first, &second] {
            let (tx, rx) = oneshot::channel();
            release.lock().unwrap().insert(id.clone(), rx);
            release_senders.insert(id.clone(), tx);
        }
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send(id.clone());
                let receiver = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                    .expect("release exists");
                let _ = receiver.await;
                result(&id)
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            2,
            None,
            None,
            session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(call_with_safety(first.as_str(), true)))
            .await
            .unwrap();
        bounded(scheduler.add(call_with_safety(second.as_str(), true)))
            .await
            .unwrap();
        bounded(started_rx.recv()).await.unwrap();
        bounded(started_rx.recv()).await.unwrap();

        let _ = release_senders.remove(&second).unwrap().send(());
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].meta.id, second);
        // A is still executing; only safe execution may be skipped by Tn.
        assert!(release_senders.contains_key(&first));
    }

    #[tokio::test]
    async fn unsafe_context_layer_applies_once_before_next_unsafe_dispatch() {
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::unbounded_channel::<(ToolUseId, String)>();
        let release = Arc::new(Mutex::new(None::<oneshot::Receiver<()>>));
        let (release_tx, release_rx) = oneshot::channel();
        *release.lock().unwrap() = Some(release_rx);
        let apply_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_apply_count = Arc::clone(&apply_count);
        let dispatch_release = Arc::clone(&release);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let apply_count = Arc::clone(&dispatch_apply_count);
            let release = Arc::clone(&dispatch_release);
            Box::pin(async move {
                let id = call.id.clone();
                let context = call
                    .tool_context_state
                    .as_ref()
                    .map(context_model)
                    .unwrap_or_default();
                let _ = started.send((id.clone(), context));
                if id.as_str() == "toolu-layer-a" {
                    let receiver = release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                        .expect("A release exists");
                    let _ = receiver.await;
                    let apply_count = Arc::clone(&apply_count);
                    let modifier = ToolInvocationContextModifier::new::<ToolUseContext, _>(
                        move |mut context| {
                            apply_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            context.options.main_loop_model = "after-a".into();
                            context
                        },
                    );
                    payload_with_modifiers(&id, vec![modifier])
                } else {
                    result(&id)
                }
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            Some(seed_context_state()),
            session_root,
            publication_lock,
            None,
        );
        let first = ToolUseId::from("toolu-layer-a");
        let second = ToolUseId::from("toolu-layer-b");
        bounded(scheduler.add(call_with_safety(first.as_str(), false)))
            .await
            .unwrap();
        assert_eq!(
            bounded(started_rx.recv()).await.unwrap(),
            (first.clone(), "seed".into())
        );
        bounded(scheduler.add(call_with_safety(second.as_str(), false)))
            .await
            .unwrap();
        let _ = release_tx.send(());
        assert_eq!(
            bounded(started_rx.recv()).await.unwrap(),
            (second, "after-a".into()),
            "unsafe layer must be folded before the actor starts B"
        );
        assert_eq!(apply_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn safe_context_layer_applies_before_queued_successor_starts() {
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::unbounded_channel::<(ToolUseId, String)>();
        let release = Arc::new(Mutex::new(None::<oneshot::Receiver<()>>));
        let (release_tx, release_rx) = oneshot::channel();
        *release.lock().unwrap() = Some(release_rx);
        let apply_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_apply_count = Arc::clone(&apply_count);
        let dispatch_release = Arc::clone(&release);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let apply_count = Arc::clone(&dispatch_apply_count);
            let release = Arc::clone(&dispatch_release);
            Box::pin(async move {
                let id = call.id.clone();
                let context = call
                    .tool_context_state
                    .as_ref()
                    .map(context_model)
                    .unwrap_or_default();
                let _ = started.send((id.clone(), context));
                if id.as_str() == "toolu-safe-chain-a" {
                    let receiver = release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                        .expect("A release exists");
                    let _ = receiver.await;
                    let apply_count = Arc::clone(&apply_count);
                    let modifier = ToolInvocationContextModifier::new::<ToolUseContext, _>(
                        move |mut context| {
                            apply_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            context.options.main_loop_model = "after-safe-a".into();
                            context
                        },
                    );
                    payload_with_modifiers(&id, vec![modifier])
                } else {
                    result(&id)
                }
            })
        });
        // Native's processQueue calls applyEndedRunLayers immediately before
        // each newly eligible execution. The cap leaves B queued behind A.
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            Some(seed_context_state()),
            session_root,
            publication_lock,
            None,
        );
        let first = ToolUseId::from("toolu-safe-chain-a");
        let second = ToolUseId::from("toolu-safe-chain-b");
        bounded(scheduler.add(call_with_safety(first.as_str(), true)))
            .await
            .unwrap();
        assert_eq!(
            bounded(started_rx.recv()).await.unwrap(),
            (first, "seed".into())
        );
        bounded(scheduler.add(call_with_safety(second.as_str(), true)))
            .await
            .unwrap();
        assert!(
            started_rx.try_recv().is_err(),
            "B remains queued at the cap"
        );

        let _ = release_tx.send(());
        assert_eq!(
            bounded(started_rx.recv()).await.unwrap(),
            (second.clone(), "after-safe-a".into()),
            "ended-run layers must reach B before its owned dispatch starts"
        );
        assert_eq!(apply_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn safe_context_layer_without_suffix_waits_for_final_drain() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(None::<oneshot::Receiver<()>>));
        let (release_tx, release_rx) = oneshot::channel();
        *release.lock().unwrap() = Some(release_rx);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send(id.clone());
                let receiver = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("release exists");
                let _ = receiver.await;
                let modifier =
                    ToolInvocationContextModifier::new::<ToolUseContext, _>(|mut context| {
                        context.options.main_loop_model = "after-final-a".into();
                        context
                    });
                payload_with_modifiers(&id, vec![modifier])
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            Some(seed_context_state()),
            session_root,
            publication_lock,
            None,
        );
        let id = ToolUseId::from("toolu-final-only-a");
        bounded(scheduler.add(call_with_safety(id.as_str(), true)))
            .await
            .unwrap();
        assert_eq!(bounded(started_rx.recv()).await, Some(id));
        let _ = release_tx.send(());
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();

        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(
            context_model(ready[0].context_state.as_ref().unwrap()),
            "seed",
            "Tn sees the pre-layer context when no queued call caused processQueue to apply it"
        );
        let final_state = bounded(scheduler.finish_normal()).await.unwrap().unwrap();
        assert_eq!(context_model(&final_state), "after-final-a");
    }

    #[tokio::test]
    async fn rejected_model_modifier_keeps_ready_results_and_reports_route_reason() {
        for concurrency_safe in [false, true] {
            let resolver: ModelContextResolver =
                Arc::new(|_, _| Err("model missing is unavailable in provider-b".into()));
            let dispatch: ToolDispatch = Arc::new(move |call| {
                Box::pin(async move {
                    let modifier =
                        ToolInvocationContextModifier::new::<ToolUseContext, _>(|mut context| {
                            context.options.main_loop_model = "missing".into();
                            context.options.model_profile = Some("provider-b".into());
                            context
                        });
                    payload_with_modifiers(&call.id, vec![modifier])
                })
            });
            let (session_root, publication_lock) = test_session_generation();
            let mut scheduler = ToolScheduler::spawn(
                dispatch,
                1,
                None,
                Some(seed_context_state()),
                session_root,
                publication_lock,
                Some(resolver),
            );
            bounded(scheduler.add(call_with_safety("toolu-invalid-model", concurrency_safe)))
                .await
                .unwrap();
            bounded(scheduler.wait_for_completed_count(1))
                .await
                .unwrap();
            let ready = bounded(scheduler.take_ready()).await.unwrap();
            assert_eq!(
                ready.len(),
                1,
                "route rejection cannot drop the tool result"
            );
            assert_eq!(
                context_model(ready[0].context_state.as_ref().unwrap()),
                "seed"
            );
            if !concurrency_safe {
                let error = ready[0].outcome.as_ref().err().unwrap().to_string();
                assert!(error.contains("missing"));
                assert!(error.contains("provider-b"));
            }
            let error = bounded(scheduler.finish_normal()).await.unwrap_err();
            assert!(matches!(error, SchedulerStopped::ModelResolution(reason)
                if reason.contains("missing") && reason.contains("provider-b")));
        }
    }

    #[tokio::test]
    async fn alias_reselection_is_projected_even_when_canonical_pair_matches_frozen_context() {
        let initial = ToolInvocationContextState::new(Arc::new(ToolUseContext::model_seed(
            "shared-model".into(),
            Some("provider-a".into()),
        )));
        let resolver: ModelContextResolver = Arc::new(|previous, mut updated| {
            assert_eq!(updated.options.main_loop_model, "balanced");
            updated.options.main_loop_model = previous.options.main_loop_model.clone();
            updated.options.model_profile = previous.options.model_profile.clone();
            Ok(updated)
        });
        let dispatch: ToolDispatch = Arc::new(move |call| {
            Box::pin(async move {
                let modifier =
                    ToolInvocationContextModifier::new::<ToolUseContext, _>(|mut context| {
                        context.options.main_loop_model = "balanced".into();
                        context.options.model_profile = None;
                        context
                    });
                payload_with_modifiers(&call.id, vec![modifier])
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            Some(initial),
            session_root,
            publication_lock,
            Some(resolver),
        );
        bounded(scheduler.add(call_with_safety("toolu-reselect", false)))
            .await
            .unwrap();
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        assert_eq!(bounded(scheduler.take_ready()).await.unwrap().len(), 1);
        let projected = bounded(scheduler.finish_normal()).await.unwrap().unwrap();
        let context = projected.downcast_arc::<ToolUseContext>().unwrap();
        assert_eq!(context.options.main_loop_model, "shared-model");
        assert_eq!(context.options.model_profile.as_deref(), Some("provider-a"));
    }

    #[tokio::test]
    async fn provider_only_modifier_is_resolved_and_published_at_finish() {
        let resolutions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&resolutions);
        let resolver: ModelContextResolver = Arc::new(move |previous, updated| {
            assert_eq!(previous.options.main_loop_model, "shared-model");
            assert_eq!(
                previous.options.model_profile.as_deref(),
                Some("provider-a")
            );
            assert_eq!(updated.options.main_loop_model, "shared-model");
            assert_eq!(updated.options.model_profile.as_deref(), Some("provider-b"));
            count.fetch_add(1, Ordering::SeqCst);
            Ok(updated)
        });
        let dispatch: ToolDispatch = Arc::new(move |call| {
            Box::pin(async move {
                let modifier =
                    ToolInvocationContextModifier::new::<ToolUseContext, _>(|mut context| {
                        context.options.model_profile = Some("provider-b".into());
                        context
                    });
                payload_with_modifiers(&call.id, vec![modifier])
            })
        });
        let initial = ToolInvocationContextState::new(Arc::new(ToolUseContext::model_seed(
            "shared-model".into(),
            Some("provider-a".into()),
        )));
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            Some(initial),
            session_root,
            publication_lock,
            Some(resolver),
        );
        bounded(scheduler.add(call_with_safety("toolu-profile-switch", false)))
            .await
            .unwrap();
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(ready.len(), 1);
        let final_state = bounded(scheduler.finish_normal()).await.unwrap().unwrap();
        let context = final_state.downcast_arc::<ToolUseContext>().unwrap();
        assert_eq!(context.options.main_loop_model, "shared-model");
        assert_eq!(context.options.model_profile.as_deref(), Some("provider-b"));
        assert_eq!(resolutions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn safe_context_layers_apply_once_in_registration_order_at_barrier() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(
            HashMap::<ToolUseId, oneshot::Receiver<()>>::new(),
        ));
        let mut release_senders = HashMap::new();
        let first = ToolUseId::from("toolu-safe-layer-a");
        let second = ToolUseId::from("toolu-safe-layer-b");
        for id in [&first, &second] {
            let (tx, rx) = oneshot::channel();
            release.lock().unwrap().insert(id.clone(), rx);
            release_senders.insert(id.clone(), tx);
        }
        let mut shared_context = ToolUseContext::model_seed("seed".into(), None);
        let request_history = vec![lingxi_core::types::ConversationMessage::user(
            MessageId::new(),
            "frozen shared request history".into(),
        )];
        shared_context.messages = request_history.clone();
        shared_context.session = Some(Arc::new(tokio::sync::Mutex::new(
            lingxi_core::session::SessionState::empty(
                lingxi_core::types::SessionId::new(),
                "seed".into(),
            ),
        )));
        shared_context.subagent_registry = Some(Arc::new(tool_api::registry::ToolRegistry::new()));
        let shared_session_ptr = shared_context
            .session
            .as_ref()
            .map(|session| Arc::as_ptr(session) as usize);
        let shared_registry_ptr = shared_context
            .subagent_registry
            .as_ref()
            .map(|registry| Arc::as_ptr(registry) as usize);
        let apply_order = Arc::new(Mutex::new(Vec::<String>::new()));
        let dispatch_order = Arc::clone(&apply_order);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            let apply_order = Arc::clone(&dispatch_order);
            let request_history = request_history.clone();
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send(id.clone());
                let receiver = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                    .expect("release exists");
                let _ = receiver.await;
                let layer = if id.as_str() == "toolu-safe-layer-a" {
                    "A"
                } else {
                    "B"
                }
                .to_string();
                let expected_session_ptr = shared_session_ptr;
                let expected_registry_ptr = shared_registry_ptr;
                let modifier =
                    ToolInvocationContextModifier::new::<ToolUseContext, _>(move |mut context| {
                        assert_eq!(context.messages, request_history);
                        assert!(context.tool_use_id.is_none());
                        assert!(context.assistant_message_id.is_none());
                        assert!(context.assistant_message.is_none());
                        assert!(context.same_turn_tool_uses.is_empty());
                        assert!(context.cancel.is_none());
                        assert_eq!(
                            context
                                .session
                                .as_ref()
                                .map(|session| Arc::as_ptr(session) as usize),
                            expected_session_ptr,
                        );
                        assert_eq!(
                            context
                                .subagent_registry
                                .as_ref()
                                .map(|registry| Arc::as_ptr(registry) as usize),
                            expected_registry_ptr,
                        );
                        apply_order
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(layer.clone());
                        context.options.main_loop_model.push_str(&layer);
                        context
                    });
                payload_with_modifiers(&id, vec![modifier])
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            2,
            None,
            Some(ToolInvocationContextState::new(Arc::new(shared_context))),
            session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(call_with_safety(first.as_str(), true)))
            .await
            .unwrap();
        bounded(scheduler.add(call_with_safety(second.as_str(), true)))
            .await
            .unwrap();
        bounded(started_rx.recv()).await.unwrap();
        bounded(started_rx.recv()).await.unwrap();

        let _ = release_senders.remove(&second).unwrap().send(());
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        assert!(
            apply_order.lock().unwrap().is_empty(),
            "A is still executing"
        );
        let _ = release_senders.remove(&first).unwrap().send(());
        bounded(scheduler.wait_for_completed_count(2))
            .await
            .unwrap();
        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(ready.len(), 2);
        let final_state = bounded(scheduler.finish_normal()).await.unwrap().unwrap();
        assert_eq!(*apply_order.lock().unwrap(), vec!["A", "B"]);
        assert_eq!(context_model(&final_state), "seedAB");
    }

    #[tokio::test]
    async fn user_interrupt_cancels_only_cancel_calls_and_drops_their_layers() {
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::unbounded_channel::<(ToolUseId, CancellationToken)>();
        let release = Arc::new(Mutex::new(None::<oneshot::Receiver<()>>));
        let (release_tx, release_rx) = oneshot::channel();
        *release.lock().unwrap() = Some(release_rx);
        let apply_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_apply_count = Arc::clone(&apply_count);
        let dispatch_release = Arc::clone(&release);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let apply_count = Arc::clone(&dispatch_apply_count);
            let release = Arc::clone(&dispatch_release);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send((id.clone(), call.cancellation.clone()));
                if id.as_str() == "toolu-interrupt-a" {
                    let receiver = release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                        .expect("A release exists");
                    // Model a non-cooperative Cancel tool returning a late
                    // context layer after its per-tool token was cancelled.
                    let _ = receiver.await;
                    let modifier =
                        ToolInvocationContextModifier::new::<ToolUseContext, _>(move |context| {
                            apply_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            context
                        });
                    payload_with_modifiers(&id, vec![modifier])
                } else {
                    result(&id)
                }
            })
        });
        let user_cancel = CancellationToken::new();
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            Some(user_cancel.clone()),
            Some(seed_context_state()),
            session_root,
            publication_lock,
            None,
        );
        let first = ToolUseId::from("toolu-interrupt-a");
        let second = ToolUseId::from("toolu-interrupt-b");
        bounded(scheduler.add(cancel_call_with_safety(first.as_str(), false)))
            .await
            .unwrap();
        let (started_id, first_cancel) = bounded(started_rx.recv()).await.unwrap();
        assert_eq!(started_id, first);
        bounded(scheduler.add(cancel_call_with_safety(second.as_str(), false)))
            .await
            .unwrap();

        user_cancel.cancel();
        bounded(first_cancel.cancelled()).await;
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        assert!(
            started_rx.try_recv().is_err(),
            "queued Cancel call never enters W1"
        );
        let _ = release_tx.send(());
        bounded(scheduler.wait_for_completed_count(2))
            .await
            .unwrap();

        assert_eq!(apply_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(
            ready
                .iter()
                .map(|entry| entry.meta.id.clone())
                .collect::<Vec<_>>(),
            vec![first, second],
            "both cancelled results retain registration order"
        );
        assert!(ready.iter().all(|entry| entry.outcome.is_err()));
    }

    #[tokio::test]
    async fn user_interrupt_keeps_block_calls_running_and_promotes_queued_block() {
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::unbounded_channel::<(ToolUseId, CancellationToken)>();
        let release = Arc::new(Mutex::new(
            HashMap::<ToolUseId, oneshot::Receiver<()>>::new(),
        ));
        let mut release_senders = HashMap::new();
        let first = ToolUseId::from("toolu-block-a");
        let second = ToolUseId::from("toolu-block-b");
        for id in [&first, &second] {
            let (tx, rx) = oneshot::channel();
            release.lock().unwrap().insert(id.clone(), rx);
            release_senders.insert(id.clone(), tx);
        }
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send((id.clone(), call.cancellation.clone()));
                let receiver = release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                    .expect("release exists");
                let _ = receiver.await;
                result(&id)
            })
        });
        let user_cancel = CancellationToken::new();
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            Some(user_cancel.clone()),
            None,
            session_root,
            publication_lock,
            None,
        );
        bounded(scheduler.add(call_with_safety(first.as_str(), false)))
            .await
            .unwrap();
        let (started_id, first_cancel) = bounded(started_rx.recv()).await.unwrap();
        assert_eq!(started_id, first);
        bounded(scheduler.add(call_with_safety(second.as_str(), false)))
            .await
            .unwrap();

        user_cancel.cancel();
        tokio::task::yield_now().await;
        assert!(
            !first_cancel.is_cancelled(),
            "Block call keeps its context token"
        );
        assert!(started_rx.try_recv().is_err(), "B remains behind running A");

        let _ = release_senders.remove(&first).unwrap().send(());
        let (next_id, next_cancel) = bounded(started_rx.recv()).await.unwrap();
        assert_eq!(next_id, second, "Block B starts after Block A completes");
        assert!(!next_cancel.is_cancelled());
        let _ = release_senders.remove(&second).unwrap().send(());
        bounded(scheduler.wait_for_completed_count(2))
            .await
            .unwrap();
        assert_eq!(
            bounded(scheduler.take_ready())
                .await
                .unwrap()
                .iter()
                .map(|entry| entry.meta.id.clone())
                .collect::<Vec<_>>(),
            vec![first, second]
        );
    }

    #[tokio::test]
    async fn terminal_error_gate_does_not_start_queued_suffix() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(None::<oneshot::Receiver<()>>));
        let (release_tx, release_rx) = oneshot::channel();
        *release.lock().unwrap() = Some(release_rx);
        let dispatch_release = Arc::clone(&release);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let release = Arc::clone(&dispatch_release);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send(id.clone());
                if id.as_str() == "toolu-error-a" {
                    let receiver = release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                        .expect("A release exists");
                    let _ = receiver.await;
                }
                result(&id)
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            None,
            session_root,
            publication_lock,
            None,
        );
        let first = ToolUseId::from("toolu-error-a");
        let second = ToolUseId::from("toolu-error-b");
        bounded(scheduler.add(call_with_safety(first.as_str(), false)))
            .await
            .unwrap();
        assert_eq!(bounded(started_rx.recv()).await, Some(first));
        bounded(scheduler.add(call_with_safety(second.as_str(), false)))
            .await
            .unwrap();

        bounded(scheduler.stop_scheduling()).await.unwrap();
        let _ = release_tx.send(());
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        assert!(
            started_rx.try_recv().is_err(),
            "terminal drain cannot promote B"
        );
        let ready = bounded(scheduler.take_all_ready()).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].meta.id, ToolUseId::from("toolu-error-a"));
    }

    #[tokio::test]
    async fn cancelled_generation_does_not_apply_unsafe_layer_or_start_queued_work() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<ToolUseId>();
        let release = Arc::new(Mutex::new(None::<oneshot::Receiver<()>>));
        let (release_tx, release_rx) = oneshot::channel();
        *release.lock().unwrap() = Some(release_rx);
        let (late_completion_tx, late_completion_rx) = oneshot::channel();
        let late_completion = Arc::new(Mutex::new(Some(late_completion_tx)));
        let apply_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_apply_count = Arc::clone(&apply_count);
        let dispatch_release = Arc::clone(&release);
        let dispatch_late_completion = Arc::clone(&late_completion);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let apply_count = Arc::clone(&dispatch_apply_count);
            let release = Arc::clone(&dispatch_release);
            let late_completion = Arc::clone(&dispatch_late_completion);
            Box::pin(async move {
                let id = call.id.clone();
                let _ = started.send(id.clone());
                if id.as_str() == "toolu-reset-a" {
                    let receiver = release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                        .expect("release exists");
                    let _ = receiver.await;
                    let apply_count = Arc::clone(&apply_count);
                    let modifier =
                        ToolInvocationContextModifier::new::<ToolUseContext, _>(move |context| {
                            apply_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            context
                        });
                    let outcome = payload_with_modifiers(&id, vec![modifier]);
                    if let Some(completed) = late_completion
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                    {
                        let _ = completed.send(());
                    }
                    outcome
                } else {
                    result(&id)
                }
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            Some(seed_context_state()),
            session_root,
            publication_lock,
            None,
        );
        let first = ToolUseId::from("toolu-reset-a");
        let queued = ToolUseId::from("toolu-reset-b");
        bounded(scheduler.add(call_with_safety(first.as_str(), false)))
            .await
            .unwrap();
        assert_eq!(bounded(started_rx.recv()).await, Some(first.clone()));
        bounded(scheduler.add(call_with_safety(queued.as_str(), false)))
            .await
            .unwrap();
        let removal = bounded(
            scheduler.reset_after_server_fallback(Some(ToolUseRemovalReason::FallbackSweep)),
        )
        .await
        .unwrap();
        assert_eq!(removal.ids, vec![first, queued]);
        let _ = release_tx.send(());
        bounded(late_completion_rx)
            .await
            .expect("old dispatch reaches its return point after reset");
        assert!(bounded(scheduler.take_ready()).await.unwrap().is_empty());
        assert!(bounded(scheduler.finish_normal()).await.unwrap().is_none());
        assert_eq!(apply_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(started_rx.try_recv().is_err(), "queued B never enters W1");
        assert!(
            bounded(scheduler.is_current_generation_idle())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn reset_after_accepted_completion_clears_ready_row_and_restores_base_context() {
        let (started_tx, mut started_rx) =
            tokio::sync::mpsc::unbounded_channel::<(ToolUseId, String)>();
        let apply_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dispatch_apply_count = Arc::clone(&apply_count);
        let dispatch: ToolDispatch = Arc::new(move |call| {
            let started = started_tx.clone();
            let apply_count = Arc::clone(&dispatch_apply_count);
            Box::pin(async move {
                let id = call.id.clone();
                let base_model = call
                    .tool_context_state
                    .as_ref()
                    .map(context_model)
                    .unwrap_or_default();
                let _ = started.send((id.clone(), base_model));
                let modifiers = if id.as_str() == "toolu-completed-before-reset" {
                    let modifier = ToolInvocationContextModifier::new::<ToolUseContext, _>(
                        move |mut context| {
                            apply_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            context.options.main_loop_model = "old-generation-layer".into();
                            context
                        },
                    );
                    vec![modifier]
                } else {
                    Vec::new()
                };
                payload_with_modifiers(&id, modifiers)
            })
        });
        let (session_root, publication_lock) = test_session_generation();
        let mut scheduler = ToolScheduler::spawn(
            dispatch,
            1,
            None,
            Some(seed_context_state()),
            session_root,
            publication_lock,
            None,
        );
        let old = ToolUseId::from("toolu-completed-before-reset");
        bounded(scheduler.add(call_with_safety(old.as_str(), false)))
            .await
            .unwrap();
        assert_eq!(
            bounded(started_rx.recv()).await.unwrap(),
            (old.clone(), "seed".into())
        );
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        assert_eq!(
            apply_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the accepted unsafe completion has already applied its one-shot modifier"
        );

        let removal = bounded(
            scheduler.reset_after_server_fallback(Some(ToolUseRemovalReason::FallbackSweep)),
        )
        .await
        .unwrap();
        assert_eq!(removal.ids, vec![old]);
        assert!(bounded(scheduler.take_ready()).await.unwrap().is_empty());
        assert!(bounded(scheduler.finish_normal()).await.unwrap().is_none());

        let rebuilt = ToolUseId::from("toolu-rebuilt-after-reset");
        bounded(scheduler.add(call_with_safety(rebuilt.as_str(), false)))
            .await
            .unwrap();
        assert_eq!(
            bounded(started_rx.recv()).await.unwrap(),
            (rebuilt.clone(), "seed".into()),
            "the new generation starts from the immutable constructor context"
        );
        bounded(scheduler.wait_for_completed_count(1))
            .await
            .unwrap();
        let ready = bounded(scheduler.take_ready()).await.unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].meta.id, rebuilt);
        assert_eq!(apply_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(bounded(scheduler.finish_normal()).await.unwrap().is_none());
    }

    #[test]
    fn tool_api_fn_once_modifier_is_wrapped_once_and_clones_share_consumption() {
        let applications = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_applications = Arc::clone(&applications);
        let tool_api_modifier: tool_api::tool_trait::ContextModifier = Box::new(move |context| {
            callback_applications.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            context
        });
        // Move the exact FnOnce into Core's opaque wrapper; never construct a
        // second wrapper from a cloned or replayed closure.
        let modifier = ToolInvocationContextModifier::new::<ToolUseContext, _>(tool_api_modifier);
        let shared = modifier.clone();
        assert!(
            modifier
                .apply(ToolUseContext::model_seed("seed".into(), None))
                .is_ok()
        );
        assert_eq!(applications.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(matches!(
            shared.apply(ToolUseContext::model_seed("seed".into(), None)),
            Err(
                lingxi_core::host::tool_invoker::ToolInvocationContextModifierError::AlreadyApplied
            )
        ));
        assert_eq!(applications.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
