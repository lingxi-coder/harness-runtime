//! `BackgroundAgentSpawner` — the composition-root decorator that makes the
//! `run_in_background` (async-agent) path LIVE end-to-end.
//!
//! `AgentTool::dispatch_async` already calls [`SubagentSpawner::spawn_async`]
//! and renders claude-code's byte-faithful `async_launched` payload; the
//! production [`agent::PoolSubagentSpawner`] leaves `spawn_async` unwired (a
//! clear error). This decorator wraps that spawner, delegates every sync method
//! verbatim, and overrides ONLY `spawn_async` to wire the full
//! `registerAsyncAgent` lifecycle by REUSING the machinery the coordinator
//! teammate already uses — no redesign:
//!
//! 1. `registry.spawn(LocalAgent{is_backgrounded:true})` → the persistent
//!    [`tasks::handlers::LocalAgentHandler`] worker (parks "comes to rest" after
//!    each turn-set, resumable via `send_message`). Returns the `task_id`.
//! 2. Register a [`TeammateMailbox`] for the advertised `AgentId` on the shared
//!    [`MailboxRouter`] (+ the optional `name`), so a `SendMessage({to})`
//!    resolves it.
//! 3. Start a [`coordinator::run_teammate_pump`] that drains that mailbox into
//!    `registry.send_message(task_id)` (the registry's [`TeamSpawnSeam`] impl →
//!    `LocalAgentHandler::send_message` → `pool.send_event`). This is the
//!    `injectUserMessageToTeammate` bridge.
//!
//! Routing chain: `SendMessage(agent_id|name)` → `MailboxRouter.route` →
//! mailbox → pump → `registry.send_message(task_id)` → handler resume →
//! `pool.send_event(agent_id)`. The pool routes by its OWN agent id; the
//! advertised `AgentId` is purely the mailbox key, consistent end-to-end.

use std::sync::Arc;

use async_trait::async_trait;
use coordinator::mailbox::{MailboxRouter, TeammateMailbox};
use coordinator::run_teammate_pump;
use hooks::HookRegistry;
use hooks::mods::{ModAgentSpawnAdmission, ModAgentSpawnStart};
use lingxi_core::host::RuntimeSpawner;
use lingxi_core::host::subagent_spawn::{
    AgentSpawnAdmission, AgentSpawnStart, AsyncLaunch, SelectedAgentMeta, SubagentInheritance,
    SubagentListingEntry, SubagentResult, SubagentSpawnError, SubagentSpawnRequest,
    SubagentSpawner,
};
use lingxi_core::host::team_spawn::TeamSpawnSeam;
use lingxi_core::types::AgentId;
use tasks::TaskType;
use tasks::registry::TaskRegistry;
use tasks::task_trait::TaskSpawnInput;
use tokio::sync::RwLock;

struct DesktopModSpawnStart(ModAgentSpawnStart);

impl AgentSpawnStart for DesktopModSpawnStart {
    fn started(self: Box<Self>, agent_id: AgentId, model: String) {
        self.0.started(agent_id.as_uuid().to_string(), model);
    }

    fn failed(self: Box<Self>, reason: String) {
        self.0.failed(reason);
    }
}

/// Decorator that wires `spawn_async` (the `run_in_background` path) while
/// delegating every synchronous `SubagentSpawner` method to `inner`.
pub struct BackgroundAgentSpawner {
    /// Live Mod host seating; read per spawn so plugin reloads take effect.
    pub mod_hooks: Option<Arc<RwLock<HookRegistry>>>,
    /// Persistent teammate service for an enabled implicit session team.
    pub teammate_spawner: Option<Arc<coordinator::ImplicitTeammateSpawner>>,
    /// The wrapped production spawner — every sync method delegates here.
    pub inner: Arc<dyn SubagentSpawner>,
    /// The concrete registry: `spawn(LocalAgent)` dispatches to the persistent
    /// handler, and it doubles as the [`TeamSpawnSeam`] the pump calls back.
    pub registry: Arc<TaskRegistry>,
    /// The process-wide mailbox router shared with the `SendMessage` tool.
    pub mailbox_router: Arc<MailboxRouter>,
    /// D17-safe task spawner for the per-agent mailbox→runner pump.
    pub runtime: Arc<dyn RuntimeSpawner>,
    /// This session's `subagents/` directory — where every background agent's
    /// transcript lives, and therefore where a forked skill's scoping sidecars
    /// are written (beside `agent-<id>.jsonl`).
    /// `None` disables fork persistence (a host with no session storage); a
    /// forking skill then still launches, and a later resume refuses it because
    /// its task record names a skill with no scoping record on disk.
    pub subagents_dir: Option<std::path::PathBuf>,
}

impl BackgroundAgentSpawner {
    /// Persist a forked skill's scoping sidecars for `agent_id`, BEFORE the
    /// agent is spawned (claude `qdd(Gc(e), A)` precedes the task creation).
    ///
    /// Order matters: an agent that exists without its scoping on disk is one
    /// a later resume must refuse, so the record lands first. The write is
    /// keyed on the AGENT's own session path, not the parent's — each
    /// background agent gets its own transcript.
    ///
    /// Returns `Err` when the record cannot be persisted; the caller aborts the
    /// launch rather than producing an unresumable agent.
    async fn persist_fork_scoping(
        &self,
        agent_id: AgentId,
        request: &SubagentSpawnRequest,
    ) -> Result<(), SubagentSpawnError> {
        let Some(skill_name) = request.forked_skill_name.as_deref() else {
            return Ok(());
        };
        let Some(dir) = self.subagents_dir.as_ref() else {
            return Ok(());
        };
        let scoping = session::forked_skill::ForkedSkillScoping {
            skill_name: skill_name.to_string(),
            attribution_name: request
                .forked_skill_attribution
                .clone()
                .unwrap_or_else(|| skill_name.to_string()),
            effort: request
                .forked_skill_effort
                .clone()
                .map(session::forked_skill::Effort::Level),
            // An EMPTY list writes no key (claude gates the spread on
            // `f.length > 0`), so it must not become `"frozenCommandDenies":[]`.
            frozen_command_denies: (!request.frozen_command_denies.is_empty())
                .then(|| request.frozen_command_denies.clone()),
        };
        // Re-validate at the write boundary. The Skill tool already checked, but
        // this is the last point before bytes hit disk, and a record that fails
        // the schema reads back as `Malformed` — a resume REFUSAL, not "no
        // scoping" — so writing one would strand the agent.
        if !scoping.is_valid() {
            return Err(SubagentSpawnError::Runtime(
                "forked-skill scoping record is unpersistable".to_string(),
            ));
        }
        let jsonl = session::forked_skill::agent_transcript_path(dir, &agent_id.to_string());
        session::forked_skill::write_fork_records(&jsonl, &scoping)
            .await
            .map_err(|e| {
                SubagentSpawnError::Runtime(format!("failed to persist forked-skill scoping: {e}"))
            })
    }

    async fn connect_agent_route(
        &self,
        agent_id: AgentId,
        task_id: &str,
        name: Option<&str>,
    ) -> Result<(), SubagentSpawnError> {
        let mailbox = Arc::new(TeammateMailbox::new(agent_id));
        self.mailbox_router
            .register(agent_id, mailbox.clone())
            .await;
        if let Some(name) = name {
            self.mailbox_router.register_name(name, agent_id).await;
        }
        // The completion `<task-notification>` carries the TASK id, and the
        // coordinator prompt tells the model to continue the agent by sending to
        // that id. Register it as an additional address so the send resolves;
        // an alias is not a display name, so this adds no `ListAgents` row and
        // no second copy of a broadcast.
        self.mailbox_router.register_alias(task_id, agent_id).await;

        let seam: Arc<dyn TeamSpawnSeam> = self.registry.clone();
        let router = self.mailbox_router.clone();
        let pump_task_id = task_id.to_string();
        let pump = Box::pin(async move {
            run_teammate_pump(mailbox, pump_task_id, seam).await;
            router.unregister(&agent_id).await;
        });
        if let Err(e) = self.runtime.spawn("bg-agent-pump", pump).await {
            self.mailbox_router.unregister(&agent_id).await;
            let _ = self.registry.kill(task_id).await;
            return Err(SubagentSpawnError::Runtime(format!(
                "failed to start background agent pump: {e}"
            )));
        }

        Ok(())
    }

    async fn update_parent_agent_keepalive(
        &self,
        parent_agent_id: AgentId,
        child_agent_id: AgentId,
        active: bool,
    ) {
        let reason = format!("agent:{}", child_agent_id.as_uuid());
        if let Err(error) = self
            .registry
            .update_agent_list_local_fact(
                parent_agent_id,
                lingxi_core::host::task_registry::AgentListLocalFactUpdate::KeepaliveReason {
                    reason,
                    active,
                },
            )
            .await
        {
            // The parent can be a main-session or teammate row rather than a
            // local-agent task. Native keepalive reasons are local-agent task
            // facts, so those owners intentionally have no row to update here.
            tracing::debug!(
                parent_agent_id = %parent_agent_id,
                child_agent_id = %child_agent_id,
                %error,
                "could not update the parent local-agent keepalive reason"
            );
        }
    }

    /// Shared async launch body. Fresh launches mint an id; cold restores pass
    /// the persisted one so transcript, mailbox, and parked-row identities stay
    /// stable across the process boundary.
    async fn spawn_async_with_id(
        &self,
        agent_id: AgentId,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        restored_task_id: Option<&str>,
    ) -> Result<AsyncLaunch, SubagentSpawnError> {
        let description = request.description.clone().unwrap_or_default();

        // A forked skill's permission scoping lands on disk before the worker.
        // Rewriting the same validated sidecars during restore is idempotent and
        // keeps the crash-safety ordering identical to a fresh launch.
        self.persist_fork_scoping(agent_id, &request).await?;

        // Native YP associates a launched Agent child with its parent before
        // the parent can come to rest. Keep this as one atomic reason update;
        // other child/task producers own their own reasons in the same set.
        let parent_agent_id = request.creator_agent_id;
        if let Some(parent_agent_id) = parent_agent_id {
            self.update_parent_agent_keepalive(parent_agent_id, agent_id, true)
                .await;
        }

        let task_id = match self
            .registry
            .spawn_with_aliases(
                TaskType::LocalAgent,
                TaskSpawnInput::LocalAgent {
                    agent_id,
                    subagent_type: request.subagent_type.clone(),
                    prompt: request.prompt.clone(),
                    is_backgrounded: true,
                    tool_use_id: request.tool_use_id.clone(),
                    creator_teammate_name: request.creator_teammate_name.clone(),
                    creator_team_name: request.creator_team_name.clone(),
                    creator_agent_id: request.creator_agent_id,
                    spawn_request: Some(request.clone()),
                    inheritance: Some(inherit),
                },
                description,
                &restored_task_id
                    .map(str::to_string)
                    .into_iter()
                    .collect::<Vec<_>>(),
            )
            .await
        {
            Ok(task_id) => task_id,
            Err(error) => {
                if let Some(parent_agent_id) = parent_agent_id {
                    self.update_parent_agent_keepalive(parent_agent_id, agent_id, false)
                        .await;
                }
                return Err(SubagentSpawnError::Runtime(error.to_string()));
            }
        };

        if let Err(error) = self
            .connect_agent_route(agent_id, &task_id, request.name.as_deref())
            .await
        {
            if let Some(parent_agent_id) = parent_agent_id {
                self.update_parent_agent_keepalive(parent_agent_id, agent_id, false)
                    .await;
            }
            return Err(error);
        }
        if let Some(old_task_id) = restored_task_id {
            self.mailbox_router
                .register_alias(old_task_id, agent_id)
                .await;
        }

        let output_file = self
            .registry
            .output_manager
            .path_for(&task_id)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| SubagentSpawnError::Runtime(e.to_string()))?;

        Ok(AsyncLaunch {
            agent_id,
            output_file,
        })
    }
}

#[async_trait]
impl SubagentSpawner for BackgroundAgentSpawner {
    fn owns_subagent_stop_hooks(
        &self,
        persistent: bool,
        session_id: Option<lingxi_core::types::SessionId>,
        scope: lingxi_core::host::subagent_spawn::SubagentStopScope,
    ) -> bool {
        self.inner
            .owns_subagent_stop_hooks(persistent, session_id, scope)
    }
    async fn resume_foreground_peer(
        &self,
        agent_id: &AgentId,
        envelope: lingxi_core::host::handback::HandbackEnvelope,
    ) -> Result<(), SubagentSpawnError> {
        self.inner.resume_foreground_peer(agent_id, envelope).await
    }

    fn decorate_tool_invoker(
        &self,
        invoker: Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker>,
    ) -> Arc<dyn lingxi_core::host::tool_invoker::ToolInvoker> {
        match &self.mod_hooks {
            Some(hooks) => Arc::new(super::mod_tool_invoker::ModSubagentToolInvoker::new(
                invoker,
                hooks.clone(),
            )),
            None => self.inner.decorate_tool_invoker(invoker),
        }
    }

    async fn begin_agent_spawn(
        &self,
        input: serde_json::Value,
        provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    ) -> Result<AgentSpawnAdmission, SubagentSpawnError> {
        let host = match &self.mod_hooks {
            Some(registry) => registry.read().await.mod_host(),
            None => None,
        };
        let Some(host) = host else {
            return self.inner.begin_agent_spawn(input, provenance).await;
        };
        match host
            .begin_agent_spawn(input, provenance)
            .await
            .map_err(|error| SubagentSpawnError::Runtime(error.to_string()))?
        {
            ModAgentSpawnAdmission::Forwarded { input, start } => {
                Ok(AgentSpawnAdmission::Forwarded {
                    input,
                    start: Box::new(DesktopModSpawnStart(start)),
                })
            }
            ModAgentSpawnAdmission::Answered(answer) => Ok(AgentSpawnAdmission::Answered(answer)),
        }
    }

    async fn resume_foreground(
        &self,
        agent_id: &AgentId,
        message: String,
    ) -> Result<(), SubagentSpawnError> {
        self.inner.resume_foreground(agent_id, message).await
    }
    async fn connect_foreground_route(
        &self,
        agent_id: AgentId,
        task_id: &str,
        name: Option<&str>,
    ) -> Result<(), SubagentSpawnError> {
        self.connect_agent_route(agent_id, task_id, name).await
    }

    fn teammate_enabled(&self) -> bool {
        self.teammate_spawner.is_some()
    }

    async fn spawn_teammate(
        &self,
        request: SubagentSpawnRequest,
        mut inherit: SubagentInheritance,
    ) -> Result<lingxi_core::host::team_spawn::TeammateLaunch, SubagentSpawnError> {
        match &self.teammate_spawner {
            Some(spawner) => {
                // AgentTool's named in-process teammate branch does not pass
                // through `decorate_tool_invoker` itself. Decorate here so a
                // teammate inheriting a parent's worktree still dispatches
                // every child tool through the Mods `tool.call` middleware.
                // The ordinary sync and async Agent paths decorate before
                // calling `spawn` / `spawn_async`, so keep this at the teammate
                // boundary only to avoid wrapping those paths twice.
                inherit.tool_invoker = self.decorate_tool_invoker(inherit.tool_invoker);
                spawner.spawn(request, inherit).await
            }
            None => Err(SubagentSpawnError::Runtime(
                "Teammate spawning is not available in this session".into(),
            )),
        }
    }

    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.inner.spawn(request, inherit).await
    }

    /// Forwarded explicitly. The trait's default chain would collapse this
    /// onto [`SubagentSpawner::spawn`] and silently DROP `progress`, so a
    /// caller that asked for nested-step progress would get none the moment
    /// this decorator is in the chain.
    async fn spawn_with_progress(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.inner
            .spawn_with_progress(request, inherit, progress)
            .await
    }

    /// Forwarded explicitly — see [`Self::spawn_with_progress`]; the default
    /// chain would drop the typed live-event `observer` as well.
    async fn spawn_with_observer(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawnObserver>>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.inner
            .spawn_with_observer(request, inherit, progress, observer)
            .await
    }

    /// Forwarded explicitly, and the load-bearing one: Fusion spawns every
    /// panel through this method and passes a `PanelAllocationObserver` that
    /// records whether the pool really handed the panel a child. If the
    /// default chain ate the observer, every bar-aborted panel would be
    /// classified `"not_dispatched"` and the Agent tool would release spawn
    /// slots for subagents that really exist. The workflow idle `watchdog`
    /// rides the same method and would be dropped with it.
    async fn spawn_workflow_with_observer(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
        progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawnObserver>>,
        watchdog: lingxi_core::host::subagent_spawn::WorkflowQueryWatchdog,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.inner
            .spawn_workflow_with_observer(request, inherit, progress, observer, watchdog)
            .await
    }

    async fn agent_listing(&self) -> Vec<SubagentListingEntry> {
        self.inner.agent_listing().await
    }

    async fn agent_listing_for_model(&self) -> Vec<SubagentListingEntry> {
        let candidates = self.inner.agent_offer_candidates().await;
        let host = if let Some(registry) = &self.mod_hooks {
            registry.read().await.mod_host()
        } else {
            None
        };
        agent::filter_agent_offer_candidates(candidates, host, agent::AgentOfferContext::default())
            .await
    }

    async fn resolve_required_mcp_servers(&self, subagent_type: &str) -> Vec<String> {
        self.inner.resolve_required_mcp_servers(subagent_type).await
    }

    async fn resolve_selection(
        &self,
        subagent_type: &str,
        model: Option<&str>,
    ) -> SelectedAgentMeta {
        self.inner.resolve_selection(subagent_type, model).await
    }

    async fn register_name(&self, name: &str, agent_id: AgentId) {
        self.inner.register_name(name, agent_id).await
    }

    async fn resolve_name(&self, name: &str) -> Option<AgentId> {
        self.inner.resolve_name(name).await
    }

    async fn spawn_async(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<AsyncLaunch, SubagentSpawnError> {
        self.spawn_async_with_id(AgentId::new(), request, inherit, None)
            .await
    }

    async fn restore_async(
        &self,
        agent_id: AgentId,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<AsyncLaunch, SubagentSpawnError> {
        self.spawn_async_with_id(agent_id, request, inherit, None)
            .await
    }

    async fn restore_async_task(
        &self,
        task_id: &str,
        agent_id: AgentId,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<AsyncLaunch, SubagentSpawnError> {
        if request.resumed_history.is_none() {
            return Err(SubagentSpawnError::Runtime(
                "stable restore requires recovered history".into(),
            ));
        }
        self.spawn_async_with_id(agent_id, request, inherit, Some(task_id))
            .await
    }

    async fn concurrent_subagent_count(&self) -> usize {
        self.inner.concurrent_subagent_count().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
    use lingxi_core::host::tool_invoker::{
        SubagentInvocationContext, ToolInvoker, ToolInvokerError,
    };
    use lingxi_core::host::{BackgroundTaskHandle, OutputStream, RuntimeError};
    use platform_posix::PosixFileSystem;
    use serde_json::json;
    use tasks::TaskType;
    use tasks::output_manager::TaskOutputManager;
    use tasks::task_trait::{Task, TaskContext, TaskError, TaskHandle};
    use test_harness::mocks::MockRuntimeSpawner;
    use tokio::task::JoinHandle;

    /// Records the `is_backgrounded` flag the decorator spawned with, returning
    /// a fixed task id (the spool path is pure `path_for`, so no I/O needed).
    /// Reports `supports_messages` and answers `send_message` with a scripted
    /// [`TaskError`] so a test can drive the pump's terminal-stop path.
    struct RecordingHandler {
        seen_backgrounded: Arc<StdMutex<Option<bool>>>,
        killed_ids: Arc<StdMutex<Vec<String>>>,
        /// When `true`, `send_message` returns [`TaskError::TerminatedTask`]
        /// (the "runner is gone" signal) so a delivered message drives the pump
        /// to stop → the decorator unregisters the mailbox.
        terminate_on_message: bool,
    }
    #[async_trait]
    impl Task for RecordingHandler {
        fn name(&self) -> &str {
            "recording"
        }
        fn task_type(&self) -> TaskType {
            TaskType::LocalAgent
        }
        async fn spawn(
            &self,
            input: TaskSpawnInput,
            _ctx: TaskContext,
        ) -> Result<TaskHandle, TaskError> {
            if let TaskSpawnInput::LocalAgent {
                is_backgrounded, ..
            } = input
            {
                *self.seen_backgrounded.lock().unwrap() = Some(is_backgrounded);
            }
            Ok(TaskHandle::new("a-bg-test-1", None))
        }
        async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
            self.killed_ids.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
        fn supports_messages(&self) -> bool {
            self.terminate_on_message
        }
        async fn send_message(
            &self,
            _task_id: &str,
            _message: String,
            _ctx: TaskContext,
        ) -> Result<(), TaskError> {
            if self.terminate_on_message {
                Err(TaskError::TerminatedTask)
            } else {
                Ok(())
            }
        }
    }

    /// Fails only the mailbox-pump spawn while otherwise behaving like the
    /// tokio-backed mock runtime.
    struct FailingPumpRuntime {
        next_id: AtomicU64,
        handles: StdMutex<HashMap<u64, JoinHandle<()>>>,
    }

    impl Default for FailingPumpRuntime {
        fn default() -> Self {
            Self {
                next_id: AtomicU64::new(1),
                handles: StdMutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl RuntimeSpawner for FailingPumpRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            if name == "bg-agent-pump" {
                return Err(RuntimeError::Internal("pump spawn failed".into()));
            }
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let handle = tokio::spawn(task);
            self.handles.lock().unwrap().insert(id, handle);
            Ok(BackgroundTaskHandle {
                task_name: name.to_string(),
                task_id: id,
            })
        }

        async fn sleep(&self, duration: std::time::Duration) {
            tokio::time::sleep(duration).await;
        }

        async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            let task = self.handles.lock().unwrap().remove(&handle.task_id);
            if let Some(task) = task {
                task.abort();
                Ok(())
            } else {
                Err(RuntimeError::NotFound(handle.task_name.clone()))
            }
        }
    }

    /// The wrapped spawner is never called on the async path — `spawn` only has
    /// to type-check (the decorator delegates the SYNC path, untested here).
    struct InertSpawner;
    #[async_trait]
    impl SubagentSpawner for InertSpawner {
        async fn spawn(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            Err(SubagentSpawnError::Internal("inert".into()))
        }
    }

    struct CountingSpawner {
        count: usize,
    }

    #[async_trait]
    impl SubagentSpawner for CountingSpawner {
        async fn spawn(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            Err(SubagentSpawnError::Internal("counting".into()))
        }

        async fn concurrent_subagent_count(&self) -> usize {
            self.count
        }
    }

    /// Records WHICH `SubagentSpawner` trait method the decorator actually
    /// reached on the wrapped spawner, and whether the observer/watchdog
    /// arguments survived the hop. A decorator that overrides only `spawn`
    /// silently drops both through the trait's default chain.
    #[derive(Default)]
    struct ForwardingProbeSpawner {
        /// Method names, in call order: "spawn" | "spawn_with_progress" |
        /// "spawn_with_observer" | "spawn_workflow_with_observer".
        calls: StdMutex<Vec<&'static str>>,
        /// `true` once a call arrived carrying `Some(observer)`.
        saw_observer: AtomicBool,
        /// The `stall_timeout_ms` of the watchdog that arrived, if any.
        saw_watchdog_ms: AtomicU64,
        /// `true` once a call arrived carrying `Some(progress)`.
        saw_progress: AtomicBool,
    }

    #[async_trait]
    impl SubagentSpawner for ForwardingProbeSpawner {
        async fn spawn(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            self.calls.lock().unwrap().push("spawn");
            Err(SubagentSpawnError::Internal("probe".into()))
        }

        async fn spawn_with_progress(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
            progress: Option<tokio::sync::mpsc::Sender<String>>,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            self.calls.lock().unwrap().push("spawn_with_progress");
            if progress.is_some() {
                self.saw_progress.store(true, Ordering::SeqCst);
            }
            Err(SubagentSpawnError::Internal("probe".into()))
        }

        async fn spawn_with_observer(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
            progress: Option<tokio::sync::mpsc::Sender<String>>,
            observer: Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawnObserver>>,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            self.calls.lock().unwrap().push("spawn_with_observer");
            if progress.is_some() {
                self.saw_progress.store(true, Ordering::SeqCst);
            }
            if observer.is_some() {
                self.saw_observer.store(true, Ordering::SeqCst);
            }
            Err(SubagentSpawnError::Internal("probe".into()))
        }

        async fn spawn_workflow_with_observer(
            &self,
            _request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
            progress: Option<tokio::sync::mpsc::Sender<String>>,
            observer: Option<Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawnObserver>>,
            watchdog: lingxi_core::host::subagent_spawn::WorkflowQueryWatchdog,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            self.calls
                .lock()
                .unwrap()
                .push("spawn_workflow_with_observer");
            if progress.is_some() {
                self.saw_progress.store(true, Ordering::SeqCst);
            }
            if observer.is_some() {
                self.saw_observer.store(true, Ordering::SeqCst);
            }
            self.saw_watchdog_ms
                .store(watchdog.stall_timeout_ms, Ordering::SeqCst);
            Err(SubagentSpawnError::Internal("probe".into()))
        }
    }

    /// An observer that only has to exist — the assertion is that the wrapped
    /// spawner SEES one, not what it publishes.
    struct InertObserver;
    #[async_trait]
    impl lingxi_core::host::subagent_spawn::SubagentSpawnObserver for InertObserver {
        async fn on_event(&self, _event: lingxi_core::host::subagent_spawn::SubagentObservation) {}
    }

    struct MockInvoker;
    #[async_trait]
    impl ToolInvoker for MockInvoker {
        async fn invoke(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            Ok(json!(null))
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct MockBudget;
    #[async_trait]
    impl BudgetEnforcerHandle for MockBudget {
        async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    struct WorktreeModSession {
        cwd: PathBuf,
        logs: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl hooks::mods::ModSessionContext for WorktreeModSession {
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }

        fn root(&self) -> PathBuf {
            self.cwd.clone()
        }

        async fn model(&self) -> String {
            "claude-sonnet".into()
        }

        async fn id(&self) -> String {
            "worktree-mod-session".into()
        }

        async fn turns(&self) -> u64 {
            1
        }

        async fn emit_mod_log(&self, plugin: &str, text: &str) {
            self.logs.lock().unwrap().push(format!("{plugin}:{text}"));
        }
    }

    #[derive(Default)]
    struct CapturingTeammateSeam {
        inherited: StdMutex<Option<(AgentId, Arc<dyn ToolInvoker>)>>,
    }

    #[async_trait]
    impl TeamSpawnSeam for CapturingTeammateSeam {
        async fn spawn_teammate(
            &self,
            _agent_id: AgentId,
            _name: String,
            _team_name: String,
            _description: String,
        ) -> Result<String, lingxi_core::host::team_spawn::TeamSpawnError> {
            Ok("captured-teammate-task".into())
        }

        async fn spawn_teammate_request(
            &self,
            agent_id: AgentId,
            _name: String,
            _team_name: String,
            _request: SubagentSpawnRequest,
            inherit: SubagentInheritance,
        ) -> Result<String, lingxi_core::host::team_spawn::TeamSpawnError> {
            *self.inherited.lock().unwrap() = Some((agent_id, inherit.tool_invoker));
            Ok("captured-teammate-task".into())
        }

        async fn resolved_model_selection(
            &self,
            _: &str,
        ) -> Result<
            lingxi_core::host::team_spawn::TeammateModelSelection,
            lingxi_core::host::team_spawn::TeamSpawnError,
        > {
            Ok(lingxi_core::host::team_spawn::TeammateModelSelection {
                model: "gpt-4o".into(),
                model_profile: Some("openai".into()),
            })
        }

        async fn kill(
            &self,
            _task_id: &str,
        ) -> Result<(), lingxi_core::host::team_spawn::TeamSpawnError> {
            Ok(())
        }

        async fn is_alive(&self, _task_id: &str) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct FailTeammatePumpRuntime {
        inner: MockRuntimeSpawner,
    }

    #[async_trait]
    impl RuntimeSpawner for FailTeammatePumpRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            if name.starts_with("teammate-pump:") {
                return Err(RuntimeError::ShuttingDown);
            }
            self.inner.spawn(name, task).await
        }

        async fn sleep(&self, duration: std::time::Duration) {
            self.inner.sleep(duration).await;
        }

        async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            self.inner.cancel(handle).await
        }
    }

    #[derive(Default)]
    struct RecordingToolInvoker {
        inputs: Arc<StdMutex<Vec<serde_json::Value>>>,
    }

    #[async_trait]
    impl ToolInvoker for RecordingToolInvoker {
        async fn invoke(
            &self,
            _name: &str,
            input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            self.inputs.lock().unwrap().push(input.clone());
            Ok(input)
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct NoopOutput;

    #[async_trait]
    impl OutputStream for NoopOutput {
        async fn emit_text(&self, _text: &str) {}

        async fn emit_tool_call(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _input: &serde_json::Value,
        ) {
        }

        async fn emit_tool_result(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _model_text: &str,
            _result: &serde_json::Value,
        ) {
        }

        async fn emit_end_turn(&self, _stop_reason: &str, _cost: &lingxi_core::host::CostSnapshot) {
        }
    }

    fn request(name: Option<&str>) -> SubagentSpawnRequest {
        SubagentSpawnRequest {
            stop_hook_scope: Default::default(),
            agent_spawn_provenance: Default::default(),
            teammate_color: None,
            subagent_type: "general-purpose".into(),
            prompt: "go".into(),
            observer: None,
            context_paths: vec![],
            description: Some("a bg agent".into()),
            model: None,
            model_profile: None,
            run_in_background: true,
            name: name.map(str::to_string),
            team_name: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            mode: None,
            isolation: None,
            cwd: None,
            worktree: None,
            fork_context_messages: None,
            instruction_context: None,
            fork_parent_system_prompt: None,
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            effort: None,
            tool_use_id: None,
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            depth: 0,
            origin_session_id: None,
            parent_model_override: None,
            parent_model_profile_override: None,
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
            handback_opt_in: false,
            parent_permission_mode: None,
            handback_enabled: None,
            restored_handback_state: None,
            restored_handback_history: Vec::new(),
            handback_ends_turn_enabled: None,
            restore_handback_start: None,
        }
    }

    /// `spawn_async` spawns a BACKGROUNDED LocalAgent through the registry,
    /// registers a mailbox (+ name) for the advertised AgentId, and returns a
    /// well-formed `AsyncLaunch` whose `output_file` is the task's spool path.
    #[tokio::test]
    async fn spawn_async_spawns_backgrounded_registers_mailbox_and_returns_launch() {
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let mut reg = TaskRegistry::new(runtime.clone(), fs, output_manager);
        let seen = Arc::new(StdMutex::new(None));
        reg.register_handler(
            TaskType::LocalAgent,
            Arc::new(RecordingHandler {
                seen_backgrounded: seen.clone(),
                killed_ids: Arc::new(StdMutex::new(Vec::new())),
                terminate_on_message: false,
            }),
        );
        let registry = Arc::new(reg);
        let mailbox_router = Arc::new(MailboxRouter::new());

        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: Arc::new(InertSpawner),
            registry,
            mailbox_router: mailbox_router.clone(),
            runtime,
            subagents_dir: None,
        };
        let inherit = SubagentInheritance {
            tool_invoker: Arc::new(MockInvoker),
            budget: Arc::new(MockBudget),
        };

        let launch = deco
            .spawn_async(request(Some("bg1")), inherit)
            .await
            .expect("spawn_async should succeed");

        // 1. Spawned BACKGROUNDED (the persistent handler path).
        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "registry.spawn carried is_backgrounded=true"
        );
        // 2. A mailbox is registered for the advertised AgentId + the name,
        //    so a `SendMessage({to})` resolves it.
        assert!(
            mailbox_router.get(&launch.agent_id).await.is_some(),
            "mailbox registered for the launched AgentId"
        );
        assert_eq!(
            mailbox_router.resolve_name("bg1").await,
            Some(launch.agent_id),
            "name → AgentId registered for SendMessage(to: name)"
        );
        // 3. The launch advertises the task's spool path.
        assert!(
            launch.output_file.contains("a-bg-test-1"),
            "output_file is the spawned task's spool path: {}",
            launch.output_file
        );
    }

    #[tokio::test]
    async fn async_agent_launch_adds_only_its_child_keepalive_reason_to_local_parent() {
        use lingxi_core::host::task_registry::TaskRegistryHandle;

        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let mut concrete_registry = TaskRegistry::new(runtime.clone(), fs, output_manager);
        concrete_registry.register_handler(
            TaskType::LocalAgent,
            Arc::new(RecordingHandler {
                seen_backgrounded: Arc::new(StdMutex::new(None)),
                killed_ids: Arc::new(StdMutex::new(Vec::new())),
                terminate_on_message: false,
            }),
        );
        let registry = Arc::new(concrete_registry);
        let parent_agent_id = AgentId::nil();
        let parent = TaskRegistryHandle::create(
            registry.as_ref(),
            lingxi_core::host::task_registry::TaskCreateInput {
                task_type: "local_agent".into(),
                description: "parent agent".into(),
            },
        )
        .await
        .unwrap();

        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: Arc::new(InertSpawner),
            registry: registry.clone(),
            mailbox_router: Arc::new(MailboxRouter::new()),
            runtime,
            subagents_dir: None,
        };
        let mut request = request(None);
        request.creator_agent_id = Some(parent_agent_id);
        let launch = deco
            .spawn_async(
                request,
                SubagentInheritance {
                    tool_invoker: Arc::new(MockInvoker),
                    budget: Arc::new(MockBudget),
                },
            )
            .await
            .unwrap();

        let records = TaskRegistryHandle::list(
            registry.as_ref(),
            lingxi_core::host::task_registry::TaskListFilter::default(),
        )
        .await
        .unwrap();
        let parent = records
            .iter()
            .find(|record| record.task_id == parent.task_id)
            .expect("the local parent remains registered");
        assert_eq!(
            parent.agent_facts.as_ref().unwrap().keepalive_reasons,
            lingxi_core::host::task_registry::FieldPresence::Value(vec![format!(
                "agent:{}",
                launch.agent_id.as_uuid()
            )]),
            "the launched child adds its raw stable AgentId reason to its local parent"
        );
    }

    #[tokio::test]
    async fn desktop_spawner_admits_an_agent_spawn_mod_before_launch() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("spawn.js");
        std::fs::write(
            &module,
            r#"let started;
        export function register(on) {
            on('agent.spawn', async ($, e, next) => {
              const inherited = JSON.stringify(next.origin) === '["parent-plugin","parent-agent"]';
              const result = await next({ ...e, prompt: e.prompt + (inherited ? ' origin-ok' : ' origin-missing') });
              started = result.agentId;
              return result;
            });
            on('tool.call', () => ({ result: { started } }));
        }"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("rewrite", dir.path(), &module, json!({}))
            .await
            .unwrap();
        let mut hooks = HookRegistry::new();
        hooks.set_mod_host(host.clone());
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let fs = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let output = Arc::new(TaskOutputManager::new(dir.path().to_path_buf(), fs.clone()));
        let spawner = BackgroundAgentSpawner {
            mod_hooks: Some(Arc::new(RwLock::new(hooks))),
            teammate_spawner: None,
            inner: Arc::new(InertSpawner),
            registry: Arc::new(TaskRegistry::new(runtime.clone(), fs, output)),
            mailbox_router: Arc::new(MailboxRouter::new()),
            runtime,
            subagents_dir: None,
        };
        let provenance = lingxi_core::host::subagent_spawn::AgentSpawnProvenance {
            hook_caller: lingxi_core::host::task_registry::FieldPresence::Value(json!(
                "parent-plugin"
            )),
            hook_origin: lingxi_core::host::task_registry::FieldPresence::Value(json!([
                "parent-plugin",
                "parent-agent"
            ])),
        };
        let admission = spawner
            .begin_agent_spawn(
                json!({
                    "tool_use_id":"tool-1", "prompt":"read", "description":"read docs",
                    "subagentType":"Explore", "provider":{"plugin":"engine","tier":"core"},
                    "parentModel":"claude-sonnet", "background":false, "fork":false
                }),
                provenance,
            )
            .await
            .unwrap();
        let AgentSpawnAdmission::Forwarded { input, start } = admission else {
            panic!("the loaded Mod must forward the Agent tool's spawn");
        };
        assert_eq!(input["prompt"], "read origin-ok");
        let child = AgentId::new();
        start.started(child, "claude-sonnet".into());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let result = host
                    .dispatch("tool.call", json!({"tool":"Read"}), |_| async {
                        Ok(json!({"fallback":true}))
                    })
                    .await
                    .unwrap();
                if result["result"]["started"] == child.as_uuid().to_string() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the Mod observes the same bare child id as the Agent result");
    }

    #[tokio::test]
    async fn teammate_tool_call_runs_once_with_the_inherited_worktree_cwd() {
        let root = tempfile::tempdir().unwrap();
        let main_cwd = root.path().join("main-session");
        let worktree_cwd = root.path().join("agent-worktree");
        std::fs::create_dir_all(&main_cwd).unwrap();
        std::fs::create_dir_all(&worktree_cwd).unwrap();

        let module = root.path().join("tool-call.js");
        std::fs::write(
            &module,
            r#"let calls = 0;
export function register(on) {
  on('tool.call', { tool: 'Read' }, async ($, e, next) => {
    calls += 1;
    const cwd = await $.session.cwd();
    await $.ui.log(`${calls}:${e.agentId}:${e.tool_use_id}:${cwd}`);
    return next({ ...e, file_path: `${cwd}/read.txt` });
  });
}"#,
        )
        .unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("worktree", root.path(), &module, json!({}))
            .await
            .unwrap();
        let session = Arc::new(WorktreeModSession {
            cwd: main_cwd,
            logs: StdMutex::new(Vec::new()),
        });
        let session_dyn: Arc<dyn hooks::mods::ModSessionContext> = session.clone();
        let mut mod_registry = HookRegistry::new();
        mod_registry.attach_mod_background_context(Arc::downgrade(&session_dyn));
        mod_registry.set_mod_host(host);

        // Let the real teammate spawn boundary capture the inherited invoker,
        // then fail only pump startup so the test owns and invokes that exact
        // captured child runtime without leaving a 30-second mailbox task.
        let runtime_impl = Arc::new(FailTeammatePumpRuntime::default());
        let runtime: Arc<dyn RuntimeSpawner> = runtime_impl;
        let team = Arc::new(
            coordinator::TeamRegistry::new(AgentId::new())
                .with_config_home(root.path().join("team-config")),
        );
        let seam = Arc::new(CapturingTeammateSeam::default());
        let teammate_spawner = Arc::new(coordinator::ImplicitTeammateSpawner::new(
            team.clone(),
            seam.clone(),
            runtime.clone(),
            Arc::new(NoopOutput),
            "session-worktree-test".into(),
        ));
        teammate_spawner.initialize().await;

        let fs = Arc::new(PosixFileSystem::new(root.path().to_path_buf()));
        let output_manager = Arc::new(TaskOutputManager::new(
            root.path().to_path_buf(),
            fs.clone(),
        ));
        let background = BackgroundAgentSpawner {
            mod_hooks: Some(Arc::new(RwLock::new(mod_registry))),
            teammate_spawner: Some(teammate_spawner),
            inner: Arc::new(InertSpawner),
            registry: Arc::new(TaskRegistry::new(runtime.clone(), fs, output_manager)),
            mailbox_router: team.mailbox_router.clone(),
            runtime,
            subagents_dir: None,
        };
        let core_inputs = Arc::new(StdMutex::new(Vec::new()));
        let result = background
            .spawn_teammate(
                SubagentSpawnRequest {
                    name: Some("researcher".into()),
                    prompt: "inspect the worktree".into(),
                    cwd: Some(worktree_cwd.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                SubagentInheritance {
                    tool_invoker: Arc::new(RecordingToolInvoker {
                        inputs: core_inputs.clone(),
                    }),
                    budget: Arc::new(MockBudget),
                },
            )
            .await;
        assert!(result.is_err(), "the test runtime stops at pump startup");

        let (agent_id, invoker) = seam
            .inherited
            .lock()
            .unwrap()
            .clone()
            .expect("the teammate spawn seam receives the decorated invoker");
        let invocation = SubagentInvocationContext {
            input_projection: None,
            cancellation_token: lingxi_core::host::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: Some(agent_id),
            origin_session_id: None,
            tool_execution_policy: Default::default(),
            agent_name: Some("researcher".into()),
            team_name: Some("session-worktree-test".into()),
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: true,
            cwd: Some(worktree_cwd.clone()),
            tool_use_id: Some("toolu_teammate_read".into()),
            assistant_message_id: None,
            depth: 1,
            observer: None,
            parent_model: Some("claude-sonnet".into()),
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            instruction_context: None,
            fork_context: None,
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        };
        invoker
            .invoke_detailed(
                "Read",
                json!({"file_path":"original.txt"}),
                invocation,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            core_inputs.lock().unwrap().as_slice(),
            &[json!({"file_path":worktree_cwd.join("read.txt")})],
            "the teammate's Mod sees and rewrites against its inherited worktree"
        );
        assert_eq!(
            session.logs.lock().unwrap().as_slice(),
            &[format!(
                "worktree:1:{}:toolu_teammate_read:{}",
                agent_id.as_uuid(),
                worktree_cwd.display()
            )],
            "the event retains the teammate identity and runs exactly once"
        );
    }

    /// The decorator must FORWARD the observer/watchdog spawn paths to `inner`
    /// rather than letting the trait's default chain collapse them onto
    /// `spawn`. Fusion passes a `PanelAllocationObserver` through
    /// `spawn_workflow_with_observer` to learn whether a panel was really
    /// allocated a child; if this decorator ever wraps the spawner fusion is
    /// built with, a dropped observer makes every bar-aborted panel misreport
    /// as never-dispatched and the Agent tool over-releases the spawn quota.
    /// The workflow watchdog is dropped by the same default chain.
    #[tokio::test]
    async fn workflow_spawn_forwards_the_observer_and_watchdog_to_inner() {
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let probe = Arc::new(ForwardingProbeSpawner::default());
        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: probe.clone(),
            registry: Arc::new(TaskRegistry::new(runtime.clone(), fs, output_manager)),
            mailbox_router: Arc::new(MailboxRouter::new()),
            runtime,
            subagents_dir: None,
        };
        let observer: Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawnObserver> =
            Arc::new(InertObserver);
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);

        let _ = deco
            .spawn_workflow_with_observer(
                request(None),
                SubagentInheritance {
                    tool_invoker: Arc::new(MockInvoker),
                    budget: Arc::new(MockBudget),
                },
                Some(tx),
                Some(observer),
                lingxi_core::host::subagent_spawn::WorkflowQueryWatchdog {
                    stall_timeout_ms: 4321,
                    max_retries: 2,
                    retry_response_body: false,
                },
            )
            .await;

        assert_eq!(
            probe.calls.lock().unwrap().as_slice(),
            &["spawn_workflow_with_observer"],
            "the decorator must reach the wrapped spawner's workflow path, not \
             collapse onto plain `spawn` through the trait default chain"
        );
        assert!(
            probe.saw_observer.load(Ordering::SeqCst),
            "the observer argument must survive the decorator hop"
        );
        assert!(
            probe.saw_progress.load(Ordering::SeqCst),
            "the progress channel must survive the decorator hop"
        );
        assert_eq!(
            probe.saw_watchdog_ms.load(Ordering::SeqCst),
            4321,
            "the workflow watchdog must survive the decorator hop"
        );
    }

    /// Same forwarding requirement for the two intermediate paths.
    #[tokio::test]
    async fn observer_and_progress_spawn_paths_forward_to_inner() {
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let probe = Arc::new(ForwardingProbeSpawner::default());
        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: probe.clone(),
            registry: Arc::new(TaskRegistry::new(runtime.clone(), fs, output_manager)),
            mailbox_router: Arc::new(MailboxRouter::new()),
            runtime,
            subagents_dir: None,
        };
        let inherit = || SubagentInheritance {
            tool_invoker: Arc::new(MockInvoker),
            budget: Arc::new(MockBudget),
        };
        let observer: Arc<dyn lingxi_core::host::subagent_spawn::SubagentSpawnObserver> =
            Arc::new(InertObserver);
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);

        let _ = deco
            .spawn_with_progress(request(None), inherit(), Some(tx))
            .await;
        let (tx2, _rx2) = tokio::sync::mpsc::channel::<String>(4);
        let _ = deco
            .spawn_with_observer(request(None), inherit(), Some(tx2), Some(observer))
            .await;
        let _ = deco.spawn(request(None), inherit()).await;

        assert_eq!(
            probe.calls.lock().unwrap().as_slice(),
            &["spawn_with_progress", "spawn_with_observer", "spawn"],
            "each decorator method must land on the SAME method of `inner`"
        );
        assert!(
            probe.saw_observer.load(Ordering::SeqCst),
            "`spawn_with_observer` must carry the observer through to `inner`"
        );
    }

    #[tokio::test]
    async fn concurrent_subagent_count_delegates_to_inner_spawner() {
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));

        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: Arc::new(CountingSpawner { count: 7 }),
            registry: Arc::new(TaskRegistry::new(runtime.clone(), fs, output_manager)),
            mailbox_router: Arc::new(MailboxRouter::new()),
            runtime,
            subagents_dir: None,
        };

        assert_eq!(deco.concurrent_subagent_count().await, 7);
    }

    /// A `context: fork` skill's permission scoping is persisted beside the new
    /// agent's own transcript, and it lands BEFORE the agent exists — an agent
    /// running without a scoping record is one the resume gate must refuse.
    #[tokio::test]
    async fn spawn_async_persists_forked_skill_scoping_beside_the_agent_transcript() {
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let mut reg = TaskRegistry::new(runtime.clone(), fs, output_manager);
        reg.register_handler(
            TaskType::LocalAgent,
            Arc::new(RecordingHandler {
                seen_backgrounded: Arc::new(StdMutex::new(None)),
                killed_ids: Arc::new(StdMutex::new(Vec::new())),
                terminate_on_message: false,
            }),
        );
        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: Arc::new(InertSpawner),
            registry: Arc::new(reg),
            mailbox_router: Arc::new(MailboxRouter::new()),
            runtime,
            subagents_dir: Some(sessions.path().to_path_buf()),
        };

        let mut req = request(None);
        req.forked_skill_name = Some("code-review".into());
        req.forked_skill_attribution = Some("reviewer".into());
        req.forked_skill_effort = Some("high".into());
        req.frozen_command_denies = vec!["Bash(rm:*)".into()];

        let launch = deco
            .spawn_async(
                req,
                SubagentInheritance {
                    tool_invoker: Arc::new(MockInvoker),
                    budget: Arc::new(MockBudget),
                },
            )
            .await
            .expect("spawn_async should succeed");

        let jsonl = session::forked_skill::agent_transcript_path(
            sessions.path(),
            &launch.agent_id.to_string(),
        );
        match session::forked_skill::read_scoping(&jsonl).await {
            session::forked_skill::ScopingStatus::Valid(s) => {
                assert_eq!(s.skill_name, "code-review");
                assert_eq!(s.attribution_name, "reviewer");
                assert_eq!(
                    s.effort,
                    Some(session::forked_skill::Effort::Level("high".into()))
                );
                assert_eq!(
                    s.frozen_command_denies.as_deref(),
                    Some(["Bash(rm:*)".to_string()].as_slice())
                );
            }
            other => panic!("expected a valid scoping record, got {other:?}"),
        }
        // The provenance marker witnesses the fork identity, so a later DELETION
        // of the scoping record is a refusal rather than an unscoped resume.
        assert_eq!(
            session::forked_skill::read_marker_skill_name(&jsonl).await,
            Some("code-review".to_string())
        );
    }

    /// A non-fork spawn writes nothing — the sidecars exist only for skills
    /// whose scoping cannot be recovered from the transcript.
    #[tokio::test]
    async fn spawn_async_writes_no_sidecars_for_an_ordinary_agent() {
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let mut reg = TaskRegistry::new(runtime.clone(), fs, output_manager);
        reg.register_handler(
            TaskType::LocalAgent,
            Arc::new(RecordingHandler {
                seen_backgrounded: Arc::new(StdMutex::new(None)),
                killed_ids: Arc::new(StdMutex::new(Vec::new())),
                terminate_on_message: false,
            }),
        );
        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: Arc::new(InertSpawner),
            registry: Arc::new(reg),
            mailbox_router: Arc::new(MailboxRouter::new()),
            runtime,
            subagents_dir: Some(sessions.path().to_path_buf()),
        };

        let launch = deco
            .spawn_async(
                request(None),
                SubagentInheritance {
                    tool_invoker: Arc::new(MockInvoker),
                    budget: Arc::new(MockBudget),
                },
            )
            .await
            .unwrap();

        let jsonl = session::forked_skill::agent_transcript_path(
            sessions.path(),
            &launch.agent_id.to_string(),
        );
        assert_eq!(
            session::forked_skill::read_scoping(&jsonl).await,
            session::forked_skill::ScopingStatus::Absent
        );
    }

    /// When the backgrounded agent terminates, the pump stops and the decorator
    /// UNREGISTERS its mailbox + name index — so a later `SendMessage` resolves
    /// to "not found" instead of silently queueing into an undrained inbox
    /// (claude-code tears down async-agent state on termination). Here a
    /// delivered message maps to `Terminated`, driving the pump to stop.
    #[tokio::test]
    async fn spawn_async_unregisters_mailbox_when_agent_terminates() {
        use coordinator::mailbox::{MessageSender, TeammateMessage};

        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(MockRuntimeSpawner::default());
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let mut reg = TaskRegistry::new(runtime.clone(), fs, output_manager);
        let seen = Arc::new(StdMutex::new(None));
        reg.register_handler(
            TaskType::LocalAgent,
            Arc::new(RecordingHandler {
                seen_backgrounded: seen.clone(),
                killed_ids: Arc::new(StdMutex::new(Vec::new())),
                // A delivered message ⇒ TerminatedTask ⇒ the pump stops.
                terminate_on_message: true,
            }),
        );
        let registry = Arc::new(reg);
        let mailbox_router = Arc::new(MailboxRouter::new());

        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: Arc::new(InertSpawner),
            registry,
            mailbox_router: mailbox_router.clone(),
            runtime,
            subagents_dir: None,
        };
        let inherit = SubagentInheritance {
            tool_invoker: Arc::new(MockInvoker),
            budget: Arc::new(MockBudget),
        };

        let launch = deco
            .spawn_async(request(Some("bg2")), inherit)
            .await
            .expect("spawn_async should succeed");

        // Precondition: the mailbox + name index are registered.
        assert!(mailbox_router.get(&launch.agent_id).await.is_some());
        assert_eq!(
            mailbox_router.resolve_name("bg2").await,
            Some(launch.agent_id)
        );

        // Deliver a message: the pump forwards it → TerminatedTask → the pump
        // stops → the wrapper unregisters the mailbox + name.
        mailbox_router
            .route(
                &launch.agent_id,
                TeammateMessage {
                    from: MessageSender::Coordinator,
                    from_name: "team-lead".to_string(),
                    content: "die".to_string(),
                    summary: None,
                    message_id: "m-1".to_string(),
                    timestamp: std::time::SystemTime::now(),
                    request_id: None,
                },
            )
            .await
            .expect("route delivers into the registered mailbox");

        // The pump runs on the mock runtime's tokio task; poll until the route
        // and name index are torn down.
        let mut gone = false;
        for _ in 0..400 {
            if mailbox_router.get(&launch.agent_id).await.is_none() {
                gone = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(gone, "mailbox unregistered after the agent terminated");
        assert_eq!(
            mailbox_router.resolve_name("bg2").await,
            None,
            "name index cleared after the agent terminated"
        );
    }

    /// If the mailbox pump cannot be started, async launch must roll back: no
    /// success result, no lingering mailbox/name route, and the just-created
    /// task is killed immediately.
    #[tokio::test]
    async fn spawn_async_rolls_back_when_pump_spawn_fails() {
        let runtime: Arc<dyn RuntimeSpawner> = Arc::new(FailingPumpRuntime::default());
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(PosixFileSystem::new(PathBuf::from(dir.path())));
        let output_manager = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let mut reg = TaskRegistry::new(runtime.clone(), fs, output_manager);
        let seen = Arc::new(StdMutex::new(None));
        let killed = Arc::new(StdMutex::new(Vec::new()));
        reg.register_handler(
            TaskType::LocalAgent,
            Arc::new(RecordingHandler {
                seen_backgrounded: seen.clone(),
                killed_ids: killed.clone(),
                terminate_on_message: false,
            }),
        );
        let registry = Arc::new(reg);
        let mailbox_router = Arc::new(MailboxRouter::new());

        let deco = BackgroundAgentSpawner {
            mod_hooks: None,
            teammate_spawner: None,
            inner: Arc::new(InertSpawner),
            registry,
            mailbox_router: mailbox_router.clone(),
            runtime,
            subagents_dir: None,
        };
        let inherit = SubagentInheritance {
            tool_invoker: Arc::new(MockInvoker),
            budget: Arc::new(MockBudget),
        };

        let err = deco
            .spawn_async(request(Some("bg-fail")), inherit)
            .await
            .expect_err("pump spawn failure must roll back the async launch");

        assert!(
            err.to_string()
                .contains("failed to start background agent pump"),
            "error should explain the rollback cause: {err}"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            Some(true),
            "the task was created before rollback"
        );
        assert!(
            mailbox_router.resolve_name("bg-fail").await.is_none(),
            "name route rolled back on failure"
        );
        assert!(
            killed.lock().unwrap().iter().any(|id| id == "a-bg-test-1"),
            "rollback must kill the just-created task"
        );
    }
}
