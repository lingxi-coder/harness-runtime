//! The engine's side of the plan-approval record: a transparent listener that
//! feeds every approved `ExitPlanMode` to the service's
//! [`PlanApprovalLog`](local_app_builder_service::plan_approval::PlanApprovalLog).
//!
//! The engine has exactly one writer of "this plan is approved": the success
//! branch of `ExitPlanMode` (`tools/plan/src/plan_mode.rs`), which only runs
//! after `PermissionGate::check_exit_plan_mode` answered `Allow`. That branch
//! returns `{plan, filePath, isAgent, …}`, the adapter lowers it to
//! `ClientEvent::ToolUseResult`, and the turn loop delivers it to the
//! registered listener.
//!
//! So this observes the approval as a listener decorator rather than as a
//! permission-gate decorator: the `PermissionGate` trait carries 22 methods
//! whose transport/policy overrides a delegating newtype would have to forward
//! verbatim (a missed forward silently degrades permission enforcement). The
//! listener has one required method and one defaulted one, and the observer is
//! transparent — it records, then always forwards.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use client::adapter::ClientEventListener;
use client::protocol::events::ClientEvent;
use local_app_builder_service::plan_approval::PlanApprovalLog;

/// Transparent listener that records every approved `ExitPlanMode`.
pub(crate) struct PlanApprovalWatcher {
    inner: Arc<dyn ClientEventListener>,
    log: Arc<PlanApprovalLog>,
    session_uuid: Arc<Mutex<String>>,
}

impl PlanApprovalWatcher {
    pub(crate) fn new(
        inner: Arc<dyn ClientEventListener>,
        log: Arc<PlanApprovalLog>,
        session_uuid: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            inner,
            log,
            session_uuid,
        }
    }

    /// Record `result_json` as an approval (or a rejection) for this process.
    fn observe(&self, result_json: &str) {
        let uuid = self
            .session_uuid
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        self.log.observe(result_json, &uuid);
    }
}

#[async_trait]
impl ClientEventListener for PlanApprovalWatcher {
    async fn on_event(&self, event: ClientEvent) {
        if let ClientEvent::ToolUseResult {
            tool,
            result_json,
            is_error,
            ..
        } = &event
        {
            // `is_error` false is the engine's own statement that the tool
            // succeeded; a denied `ExitPlanMode` returns an error payload, so it
            // never reaches `observe`.
            if tool == "ExitPlanMode" && !*is_error {
                self.observe(result_json);
            }
        }
        self.inner.on_event(event).await;
    }

    async fn on_workflow_progress(
        &self,
        origin_session_id: String,
        task_id: String,
        run_id: String,
        progress: client::protocol::listings::WorkflowProgressDto,
    ) {
        self.inner
            .on_workflow_progress(origin_session_id, task_id, run_id, progress)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_app_builder_service::plan_approval::test_support::{exit_result, good_plan};

    #[tokio::test]
    async fn the_watcher_forwards_every_event_and_records_the_approval() {
        #[derive(Default)]
        struct Recorder {
            seen: Mutex<Vec<String>>,
        }
        #[async_trait]
        impl ClientEventListener for Recorder {
            async fn on_workflow_progress(
                &self,
                _: String,
                _: String,
                _: String,
                _: client::protocol::listings::WorkflowProgressDto,
            ) {
            }
            async fn on_event(&self, event: ClientEvent) {
                let label = match event {
                    ClientEvent::TextDelta { text } => text,
                    ClientEvent::ToolUseResult { tool, .. } => tool,
                    other => format!("{other:?}"),
                };
                self.seen.lock().expect("lock").push(label);
            }
        }

        let recorder = Arc::new(Recorder::default());
        let log = Arc::new(PlanApprovalLog::default());
        let session = Arc::new(Mutex::new("s1".to_string()));
        let watcher = PlanApprovalWatcher::new(recorder.clone(), log.clone(), session);

        let plan = good_plan();
        watcher
            .on_event(ClientEvent::ToolUseResult {
                id: "t1".to_string(),
                tool: "ExitPlanMode".to_string(),
                result_json: exit_result(&plan, "/tmp/p.md"),
                is_error: false,
                display: None,
            })
            .await;
        // A denied exit carries an error payload and must not be recorded.
        watcher
            .on_event(ClientEvent::ToolUseResult {
                id: "t2".to_string(),
                tool: "ExitPlanMode".to_string(),
                result_json: serde_json::json!({ "error": "denied" }).to_string(),
                is_error: true,
                display: None,
            })
            .await;
        watcher
            .on_event(ClientEvent::TextDelta {
                text: "hello".to_string(),
            })
            .await;

        // Transparent: every event still reached the wrapped listener, in order.
        assert_eq!(
            *recorder.seen.lock().expect("lock"),
            vec![
                "ExitPlanMode".to_string(),
                "ExitPlanMode".to_string(),
                "hello".to_string(),
            ]
        );
        let recorded = log
            .claim("/tmp/p.md", "s1", "app-1")
            .expect("the approved plan claims for its own conversation");
        assert_eq!(recorded.template_id.as_deref(), Some("react-dom-tabs"));
    }
}
