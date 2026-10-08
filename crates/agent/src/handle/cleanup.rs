use crate::pool::StateMachinePool;
use lingxi_core::host::subagent_spawn::SubagentObservation;
use lingxi_core::types::AgentId;
use std::sync::Arc;

/// Owns the MCP cleanup handles [`PoolSubagentSpawner::build_subagent_context`]
/// just produced — it CONNECTS the definition's inline `mcpServers`, so these
/// are LIVE connections from the moment it returns — across every await where
/// nothing else owns them, so a dropped spawn future can never orphan one.
///
/// [`SpawnDeallocGuard`] covers only the window between `pool.allocate`
/// returning and the normal terminal path's `run_agent_mcp_cleanups` call. Two
/// awaits sit outside it and both leaked [round-5 findings 11 and 19]:
/// `pool.allocate(...).await` itself (it suspends on `runtime.spawn` and on
/// `slots.write()`) runs BEFORE that guard exists, and
/// `pool.deallocate(...).await` runs AFTER it was disarmed and its vector
/// `mem::take`n empty. This guard is armed the instant the handles exist and
/// hands them on — via [`McpCleanupGuard::take`], in the very expression that
/// consumes them — to whoever owns them next.
///
/// Its `Drop` mirrors `SpawnDeallocGuard`'s: best-effort teardown on the
/// current runtime, nothing to do once no runtime is left. It emits no
/// observation of its own, so the "exactly one terminal event per spawn"
/// contract is untouched — the windows it covers are either before any
/// terminal event is possible (`allocate`) or after the normal path already
/// emitted one (`deallocate`) — and so is round-3 finding B1's ordering
/// (terminal event FIRST, MCP teardown second).
pub(super) struct McpCleanupGuard {
    pub(super) cleanups: Vec<crate::agent_mcp_tools::AgentMcpCleanupHandle>,
    pub(super) agent_type: String,
}

impl McpCleanupGuard {
    pub(super) fn new(
        cleanups: Vec<crate::agent_mcp_tools::AgentMcpCleanupHandle>,
        agent_type: String,
    ) -> Self {
        Self {
            cleanups,
            agent_type,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.cleanups.is_empty()
    }

    /// Hand the handles to their next owner. Call this ONLY in the expression
    /// that immediately consumes them (a struct field, or the argument of the
    /// `run_agent_mcp_cleanups` call being awaited on that same statement):
    /// the guard is left empty, so from here on its `Drop` is a no-op.
    pub(super) fn take(&mut self) -> Vec<crate::agent_mcp_tools::AgentMcpCleanupHandle> {
        std::mem::take(&mut self.cleanups)
    }
}

impl Drop for McpCleanupGuard {
    fn drop(&mut self) {
        if self.cleanups.is_empty() {
            return;
        }
        let cleanups = std::mem::take(&mut self.cleanups);
        let agent_type = std::mem::take(&mut self.agent_type);
        // Best-effort, exactly like `SpawnDeallocGuard::drop`: hand the async
        // teardown to the current runtime; with no runtime active (shutdown)
        // there is nothing left to disconnect from.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                crate::agent_mcp_tools::run_agent_mcp_cleanups(cleanups, &agent_type).await;
            });
        }
    }
}

/// Grace period persistent `stop` and [`SpawnDeallocGuard`]'s early-drop path
/// give the runner to observe a cooperative exit — reach its own `record_terminal`
/// transcript write and return on its own — before the hard `abort()`
/// fallback. Kept short: this directly extends how long a caller that dropped
/// the spawn future (Fusion panel timeout/cancel racing
/// `spawn_workflow_with_observer`, or any other future combinator race) waits
/// for cleanup. A named constant rather than a magic literal so the ceiling is
/// easy to find and retune (G007 / F012).
pub(super) const SPAWN_CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Cancel-safety guard for [`PoolSubagentSpawner::spawn`]. The runner runs as a
/// DETACHED pool task (`allocate` spawns it via the `RuntimeSpawner`); the
/// `spawn` future only pumps events and calls `deallocate` on the terminal
/// event. If a caller races `spawn` against a `CancellationToken` and DROPS the
/// future before that terminal event (timeout / cancel), `deallocate` would
/// never run and the detached runner would keep executing tools + leak its
/// slot.
///
/// On early drop this guard first delivers a cooperative `UserInterrupt` and
/// gives the runner [`SPAWN_CANCEL_GRACE`] to reach its own terminal state (the
/// runner's turn loop races `event_rx` against the in-flight model call and,
/// on `UserInterrupt`, writes `record_terminal("cancelled")` before returning
/// — see `runner::emit_killed`) rather than hard-aborting it mid-turn, which
/// would leave `agent-<id>.jsonl` stuck reporting `"status":"running"`
/// forever. Only after the grace elapses does it fall back to
/// `deallocate`/`abort()`. Because the dropped `spawn` future never reaches
/// its own normal terminal-emit path (`observer_events.emit_terminal` below),
/// this guard also emits the caller-visible `Killed` observation itself, so
/// observers still see exactly one terminal lifecycle event per spawn. The
/// guard is disarmed on the normal terminal path, where all of this already
/// happened inline.
///
/// The guard also owns `mcp_cleanups`/`agent_type`: the normal terminal path
/// tears down exactly the MCP connections this spawn newly created via
/// `run_agent_mcp_cleanups` (§24b), and the early-drop path must mirror that
/// — otherwise a cancelled subagent (Esc mid-`Agent(...)`, a Fusion panel
/// the panel-bar `join_set.abort_all()` drops, `panel_total_timeout`, …)
/// leaks every MCP connection its spawn opened, since nothing else ever
/// reaches those handles once the future is dropped [round-3 finding 17].
pub(super) struct SpawnDeallocGuard {
    pub(super) pool: Arc<StateMachinePool>,
    pub(super) agent_id: AgentId,
    pub(super) observer_events: crate::api::ObserverEventSink,
    pub(super) armed: bool,
    pub(super) startup_error: Option<String>,
    pub(super) agent_spawn_token: Option<lingxi_core::host::agent_statistics::AgentSpawnToken>,
    pub(super) mcp_cleanups: Vec<crate::agent_mcp_tools::AgentMcpCleanupHandle>,
    pub(super) agent_type: String,
}

impl Drop for SpawnDeallocGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(token) = &self.agent_spawn_token {
            if self.startup_error.is_some() { token.failed(); }
            else { token.killed(lingxi_core::host::agent_statistics::AgentKillReason::User); }
        }
        // The cleanup below is async; hand it to the current runtime
        // best-effort. If no runtime is active (shutdown) there is nothing
        // left to clean up.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pool = self.pool.clone();
            let id = self.agent_id;
            let observer_events = self.observer_events.clone();
            let mcp_cleanups = std::mem::take(&mut self.mcp_cleanups);
            let agent_type = std::mem::take(&mut self.agent_type);
            let startup_error = self.startup_error.take();
            handle.spawn(async move {
                // claude-code `Cre`: the agent is stopping but has NOT stopped.
                // `UserInterrupt` is cooperative and the runner only races it at
                // the model round-trip, so a runner part-way through one turn's
                // `tool_use` blocks keeps dispatching them for up to
                // `SPAWN_CANCEL_GRACE`. Close the spawn gate for that window so
                // it cannot launch work that would outlive it. Held until after
                // `deallocate` below, which is this port's settle point.
                let _stop_pending =
                    lingxi_core::host::agent_processes::mark_stop_pending(&id.to_string());
                // Best-effort: a slot that is already gone (naturally
                // completed, or raced by another deallocate) makes this a
                // no-op — `send_event` and `deallocate` are both graceful on
                // a missing agent id.
                let _ = pool
                    .send_event(&id, lingxi_core::Event::UserInterrupt)
                    .await;
                // [round-3 finding 28] Poll down the fixed grace instead of
                // blindly sleeping the whole window: the runner typically
                // reacts to `UserInterrupt` within a turn or two, and
                // holding the pool slot (and the capacity permit stored
                // inside it) any longer than that would let
                // cancelled-but-already-finished spawns starve the
                // concurrency cap for the next `Agent`/Fusion panel spawn.
                let mut elapsed = std::time::Duration::ZERO;
                let poll_interval = std::time::Duration::from_millis(50);
                while elapsed < SPAWN_CANCEL_GRACE {
                    if pool.agent_runner_finished(&id).await {
                        break;
                    }
                    tokio::time::sleep(poll_interval).await;
                    elapsed += poll_interval;
                }
                let _ = pool.deallocate(&id).await;
                // Emit the caller-visible terminal observation BEFORE
                // running MCP teardown, mirroring the normal terminal
                // path's ordering (see "Normal terminal path" above): a
                // wedged MCP `disconnect` must not be able to block the
                // `Killed` observation forever [round-3 finding B1 — a
                // regression introduced while fixing finding 17, which put
                // the cleanup await before this emit].
                observer_events.emit_terminal(match startup_error {
                    Some(error) => SubagentObservation::Failed {
                        agent_id: id,
                        error,
                    },
                    None => SubagentObservation::Killed { agent_id: id },
                });
                // §24b: mirror the normal terminal path's
                // `run_agent_mcp_cleanups` call so a spawn whose future is
                // dropped before reaching that line does not leak the MCP
                // connections it newly created.
                crate::agent_mcp_tools::run_agent_mcp_cleanups(mcp_cleanups, &agent_type).await;
            });
        }
    }
}
