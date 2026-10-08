//! Bridge a teammate's [`tasks::handlers::TaskStatusSink`] transitions onto
//! [`TeamRegistry::update_status`].
//!
//! The in-process teammate handler reports lifecycle transitions through the
//! narrow [`TaskStatusSink`] seam (it does not hold a [`TeamRegistry`]
//! reference). [`CoordinatorStatusSink`] adapts those transitions back onto the
//! coordinator's [`TeamRegistry`], scoped to the transitions the sink can
//! actually drive, and (re-)pushes the live active-worker count to the
//! orchestrator-facing [`OutputStream`] after each state change.
//!
//! A persistent runner emits a `SubagentEvent::Completed` per turn-set, but the
//! teammate handler deliberately does not forward those as `TaskStatus` values.
//! Therefore a `TaskStatus::Completed` reaching this sink is an explicit
//! terminal lifecycle decision (currently `TeammateIdle continue:false`) and
//! must map to [`WorkerStatus::Completed`].

use crate::team_registry::{TeamRegistry, WorkerStatus};
use async_trait::async_trait;
use lingxi_core::host::OutputStream;
use std::sync::Arc;
use tasks::handlers::TaskStatusSink;
use tasks::state::TaskStatus;

/// The fixed activity label used when a teammate transitions to `Running`.
///
/// [`WorkerStatus`] derives `PartialEq` over the `activity` string and the
/// [`TaskStatusSink`] carries no per-turn activity text, so a fixed label keeps
/// the mapping deterministic. Richer per-turn text is a follow-up.
const RUNNING_ACTIVITY: &str = "running";

/// The fixed error message recorded when a teammate transitions to `Failed`.
///
/// [`TaskStatus::Failed`] is a payload-less enum variant — the sink does not
/// carry the originating error string — so we record a fixed sentinel. The
/// authoritative error text remains observable on the task's spool/state.
const FAILED_ERROR: &str = "teammate task failed";

/// Adapts [`TaskStatusSink`] transitions onto a coordinator's
/// [`TeamRegistry`], then pushes the live active-worker count downstream.
pub struct CoordinatorStatusSink {
    team: Arc<TeamRegistry>,
    output: Arc<dyn OutputStream>,
}

impl CoordinatorStatusSink {
    /// Construct a sink bridging `team` and pushing status updates to `output`.
    #[must_use]
    pub fn new(team: Arc<TeamRegistry>, output: Arc<dyn OutputStream>) -> Self {
        Self { team, output }
    }

    /// Map a [`TaskStatus`] to the [`WorkerStatus`] it drives, if any.
    ///
    /// `Pending`/`Paused` produce no worker transition.
    fn worker_status_for(status: TaskStatus) -> Option<WorkerStatus> {
        match status {
            TaskStatus::Running => Some(WorkerStatus::Working {
                activity: RUNNING_ACTIVITY.to_string(),
            }),
            TaskStatus::Failed => Some(WorkerStatus::Failed {
                error: FAILED_ERROR.to_string(),
            }),
            TaskStatus::Killed => Some(WorkerStatus::Killed),
            TaskStatus::Completed => Some(WorkerStatus::Completed),
            // No transition: Pending predates the worker link and Paused has no
            // teammate analogue.
            TaskStatus::Pending | TaskStatus::Paused => None,
        }
    }
}

/// claude-code 2.1.198 caps the failed idle notification's `failureReason` at
/// 200 chars before it reaches the leader (`zTt`:
/// `kd(t.failureReason).slice(0,RXn)` @215144889, `RXn=200` @215149471).
const FAILURE_REASON_CAP: usize = 200;

impl CoordinatorStatusSink {
    async fn publish_worker(&self, task_id: &str) {
        if let Some(worker) = self.team.find_by_task_id(task_id).await {
            self.output
                .emit_coordinator_worker(&crate::handle::worker_info(worker))
                .await;
        }
    }

    /// Apply `status` to the worker linked to `task_id` and push the freshly
    /// computed active-worker count downstream. A status arriving during the
    /// activation→link window is retained by [`TeamRegistry`] and replayed when
    /// the teammate spawner publishes the task id; genuinely unknown ids remain a no-op.
    async fn apply(&self, task_id: &str, worker_status: WorkerStatus) {
        if !self
            .team
            .update_status_from_handler_by_task_id(task_id, worker_status)
            .await
        {
            return;
        }

        self.publish_worker(task_id).await;

        // PUSH the freshly-computed active-worker count + team name downstream.
        let active = self.team.active_worker_count().await;
        let team_name = self.team.team_name().await;
        self.output
            .emit_coordinator_status(active, team_name.as_deref())
            .await;
    }
}

#[async_trait]
impl TaskStatusSink for CoordinatorStatusSink {
    async fn set_teammate_idle(&self, task_id: &str) {
        self.apply(task_id, WorkerStatus::Idle).await;
    }

    async fn set_awaiting_plan_approval(&self, task_id: &str, awaiting: bool) {
        self.team
            .set_awaiting_plan_approval(task_id, awaiting)
            .await;
        self.publish_worker(task_id).await;
        let team = self.team.team_name().await;
        self.output
            .emit_coordinator_status(self.team.active_worker_count().await, team.as_deref())
            .await;
    }

    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        let Some(worker_status) = Self::worker_status_for(status) else {
            return;
        };
        self.apply(task_id, worker_status).await;
    }

    /// A teammate dying on an API error reports "failed" to the lead WITH the
    /// real failure reason (claude-code 2.1.198: the in-process runner's catch
    /// sends `{idleReason:"failed", completedStatus:"failed", failureReason}`
    /// to the leader, binary @216293689), instead of the payload-less
    /// [`FAILED_ERROR`] sentinel `set_status` records. The reason is capped at
    /// [`FAILURE_REASON_CAP`] chars, mirroring the binary's `.slice(0,200)`.
    async fn set_failed(&self, task_id: &str, error: &str) {
        let reason: String = error.chars().take(FAILURE_REASON_CAP).collect();
        self.apply(task_id, WorkerStatus::Failed { error: reason })
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::CostSnapshot;
    use lingxi_core::types::AgentId;
    use std::sync::Mutex as StdMutex;

    /// Spy [`OutputStream`] recording every `emit_coordinator_status` call as
    /// `(active_workers, team)`. All other emit methods are no-ops.
    #[derive(Default)]
    struct SpyOutput {
        statuses: StdMutex<Vec<(u32, Option<String>)>>,
        workers: StdMutex<Vec<lingxi_core::host::team_registry::WorkerInfo>>,
    }

    impl SpyOutput {
        fn last(&self) -> Option<(u32, Option<String>)> {
            self.statuses.lock().unwrap().last().cloned()
        }
        fn calls(&self) -> usize {
            self.statuses.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl OutputStream for SpyOutput {
        async fn emit_text(&self, _text: &str, _utf16_code_units: Option<&[u16]>) {}
        async fn emit_tool_call(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _input: &serde_json::Value,
         _input_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>) {
        }
        async fn emit_tool_result(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _model_text: &str,
            _result: &serde_json::Value,
         _projection: Option<&lingxi_core::host::ToolResultProjection>) {
        }
        async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}
        async fn emit_coordinator_worker(
            &self,
            worker: &lingxi_core::host::team_registry::WorkerInfo,
        ) {
            self.workers.lock().unwrap().push(worker.clone());
        }
        async fn emit_coordinator_status(&self, active_workers: u32, team: Option<&str>) {
            self.statuses
                .lock()
                .unwrap()
                .push((active_workers, team.map(str::to_string)));
        }
    }

    /// Build a registry with one worker linked to `task_id`, returning the
    /// registry, the spy output, and the constructed sink.
    async fn fixture(task_id: &str) -> (Arc<TeamRegistry>, Arc<SpyOutput>, CoordinatorStatusSink) {
        let team = Arc::new(TeamRegistry::new(AgentId::new()));
        let agent_id = team
            .spawn_worker("explorer".into(), "alpha".into(), String::new())
            .await
            .unwrap();
        team.set_task_id(&agent_id, task_id.into()).await;
        let output = Arc::new(SpyOutput::default());
        let sink = CoordinatorStatusSink::new(team.clone(), output.clone());
        (team, output, sink)
    }

    fn status_of(workers: &[crate::team_registry::WorkerAgent]) -> &WorkerStatus {
        &workers[0].status
    }

    #[tokio::test]
    async fn persistent_turn_idle_and_wake_push_live_roster() {
        let (team, output, sink) = fixture("persistent").await;
        sink.set_status("persistent", TaskStatus::Running).await;
        sink.set_teammate_idle("persistent").await;
        assert_eq!(team.list().await[0].status, WorkerStatus::Idle);
        sink.set_status("persistent", TaskStatus::Running).await;
        let rows = output.workers.lock().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].status, "idle");
        assert_eq!(rows[2].status, "working");
    }

    #[tokio::test]
    async fn running_maps_to_working() {
        let (team, _out, sink) = fixture("task-1").await;

        sink.set_status("task-1", TaskStatus::Running).await;

        let workers = team.list().await;
        assert_eq!(
            status_of(&workers),
            &WorkerStatus::Working {
                activity: "running".into()
            }
        );
    }

    #[tokio::test]
    async fn completed_maps_to_terminal_completed() {
        let (team, _out, sink) = fixture("task-1").await;

        // First, transition to Working via Running.
        sink.set_status("task-1", TaskStatus::Running).await;
        // Per-turn-set Completed events are filtered by the handler. A
        // TaskStatus::Completed reaching this sink is therefore terminal.
        sink.set_status("task-1", TaskStatus::Completed).await;

        let workers = team.list().await;
        assert_eq!(status_of(&workers), &WorkerStatus::Completed);
    }

    /// cc 2.1.198 (M9): a teammate dying on an API error reports "failed" to
    /// the lead WITH the real failure reason — `set_failed` (fed by the
    /// teammate worker's `SubagentEvent::Failed { error }`) records
    /// `WorkerStatus::Failed { error: <reason> }` on the lead's team registry,
    /// not the payload-less sentinel, and pushes the status downstream. The
    /// reason is capped at 200 chars (binary `RXn=200`,
    /// `kd(t.failureReason).slice(0,RXn)`).
    #[tokio::test]
    async fn set_failed_reports_reason_to_lead_capped_at_200() {
        let (team, out, sink) = fixture("task-api-err").await;
        sink.set_status("task-api-err", TaskStatus::Running).await;

        sink.set_failed("task-api-err", "subagent api error: 529 overloaded")
            .await;

        // The lead's view (team registry) shows Failed with the REAL reason —
        // not silence, not Running, not the FAILED_ERROR sentinel.
        assert_eq!(
            status_of(&team.list().await),
            &WorkerStatus::Failed {
                error: "subagent api error: 529 overloaded".into()
            }
        );
        // The terminal transition was pushed downstream (active count drops).
        assert_eq!(out.last(), Some((0, None)));

        // A reason longer than 200 chars is truncated to exactly 200.
        let (team, _out, sink) = fixture("task-long").await;
        let long = "e".repeat(300);
        sink.set_failed("task-long", &long).await;
        match status_of(&team.list().await) {
            WorkerStatus::Failed { error } => {
                assert_eq!(error.len(), 200, "reason capped at 200 chars");
                assert_eq!(error, &"e".repeat(200));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_and_killed_map_through() {
        // Failed.
        let (team, _out, sink) = fixture("task-f").await;
        sink.set_status("task-f", TaskStatus::Failed).await;
        assert_eq!(
            status_of(&team.list().await),
            &WorkerStatus::Failed {
                error: FAILED_ERROR.into()
            }
        );

        // Killed.
        let (team, _out, sink) = fixture("task-k").await;
        sink.set_status("task-k", TaskStatus::Killed).await;
        assert_eq!(status_of(&team.list().await), &WorkerStatus::Killed);
    }

    #[tokio::test]
    async fn unknown_task_id_is_noop() {
        let (team, out, sink) = fixture("task-1").await;

        // No worker is linked to "ghost": must not panic, must not transition,
        // and must not push a status event.
        sink.set_status("ghost", TaskStatus::Running).await;

        let workers = team.list().await;
        assert_eq!(status_of(&workers), &WorkerStatus::Idle);
        assert_eq!(out.calls(), 0, "no emit for an unknown task id");
    }

    #[tokio::test]
    async fn startup_failure_is_replayed_after_task_id_link() {
        let team = Arc::new(TeamRegistry::new(AgentId::new()));
        let agent_id = team
            .spawn_worker("explorer".into(), "alpha".into(), String::new())
            .await
            .unwrap();
        let output = Arc::new(SpyOutput::default());
        let sink = CoordinatorStatusSink::new(team.clone(), output.clone());

        sink.set_failed("task-startup", "provider allocation failed")
            .await;
        assert_eq!(status_of(&team.list().await), &WorkerStatus::Idle);
        assert_eq!(output.calls(), 0, "an unlinked status is not pushed early");

        team.set_task_id(&agent_id, "task-startup".into()).await;
        assert_eq!(
            status_of(&team.list().await),
            &WorkerStatus::Failed {
                error: "provider allocation failed".into()
            },
            "linking the handler task replays its retained failure reason"
        );
    }

    #[tokio::test]
    async fn emit_fires_with_active_count() {
        let (_team, out, sink) = fixture("task-1").await;

        sink.set_status("task-1", TaskStatus::Running).await;

        // The single worker is now Working (non-terminal) → active count 1,
        // and the team name is unset in this fixture.
        assert_eq!(out.last(), Some((1, None)));
        assert_eq!(out.calls(), 1);
    }

    #[tokio::test]
    async fn emit_carries_team_name() {
        let (team, out, sink) = fixture("task-1").await;
        team.set_team_name(Some("alpha".into())).await;

        sink.set_status("task-1", TaskStatus::Running).await;

        assert_eq!(out.last(), Some((1, Some("alpha".to_string()))));
    }
    #[tokio::test]
    async fn plan_wait_changes_push_roster_rows_without_overwriting_idle() {
        let (_team, output, sink) = fixture("task-plan").await;
        sink.set_awaiting_plan_approval("task-plan", true).await;
        sink.set_awaiting_plan_approval("task-plan", false).await;
        let rows = output.workers.lock().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].awaiting_plan_approval);
        assert!(!rows[1].awaiting_plan_approval);
        assert_eq!(rows[0].status, "idle");
        assert_eq!(rows[1].status, "idle");
    }
}
