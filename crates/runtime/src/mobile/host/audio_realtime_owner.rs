//! Native audio uses the connection's existing turn and permission ownership.
use super::*;
use llm_runtime::services::sdk::realtime::{RealtimeControl, RealtimeEvent, RealtimeEvents};
use orchestrator::{
    OrchestratorError, RealtimeAgentContext, RealtimeAgentEnd, RealtimeAgentInput,
    RealtimeAgentLimits,
};
use tokio::sync::{mpsc, oneshot};

impl MobileEngineHandle {
    /// Run native audio under the same owner as an ordinary streamed Agent turn.
    /// Cancellation waits for shared tool policy, including non-abortable mutations.
    pub async fn run_realtime_agent_owned(
        &self,
        prepared: RealtimeAgentContext,
        control: RealtimeControl,
        events: RealtimeEvents,
        inputs: mpsc::Receiver<RealtimeAgentInput>,
        output: mpsc::Sender<RealtimeEvent>,
        limits: RealtimeAgentLimits,
        cancel: CancellationToken,
    ) -> Result<RealtimeAgentEnd, OrchestratorError> {
        let transition = self.loop_transition.lock().await;
        let turn = self
            .reserve_turn(None)
            .await
            .map_err(|error| OrchestratorError::Internal(error.to_string()))?;
        self.cancel_reason.reset();
        self.inner.orchestrator.turn_span().reset();
        self.message_queue
            .register_active_turn(turn.cancel.clone())
            .await;
        self.inner.message_output.reset_message_buffer().await;
        TurnEventEmitter::new(self.event_sink.clone())
            .emit_turn_started(None)
            .await;
        let orch = self.inner.orchestrator.clone();
        let sink = self.event_sink.clone();
        let active_cancel = self.active_cancel.clone();
        let permission_gate = self.inner.permission_gate.clone();
        let message_queue = self.message_queue.clone();
        let questions = self.ask_user_question_broker.clone();
        let task_turn = turn.clone();
        let (mut result_tx, result_rx) = oneshot::channel();
        let task = self.runtime.spawn(async move {
            let audio_cancel = task_turn.cancel.child_token();
            let run = orch.run_realtime_agent(
                prepared,
                control,
                events,
                inputs,
                output,
                limits,
                audio_cancel.clone(),
            );
            tokio::pin!(run);
            let completed = tokio::select! { biased;
                _ = cancel.cancelled() => { task_turn.cancel.cancel(); None },
                _ = result_tx.closed() => { task_turn.cancel.cancel(); None },
                _ = audio_cancel.cancelled() => None,
                result = &mut run => Some(result),
            };
            let result = match completed {
                Some(result) => result,
                None => {
                    if let Some(owner) = task_turn.permission_owner_id {
                        permission_gate.cancel_owner(owner).await;
                    }
                    run.await
                }
            };
            questions.cancel_closed().await;
            if let Err(error) = &result {
                sink.emit(client::adapter::map_orchestrator_error(error))
                    .await;
            } else {
                let cancelled = task_turn.cancel.is_cancelled()
                    || matches!(result, Ok(RealtimeAgentEnd::Cancelled));
                sink.emit(ClientEvent::TurnEnded {
                    outcome: if cancelled {
                        client::protocol::events::TurnOutcomeDto::Cancelled
                    } else {
                        client::protocol::events::TurnOutcomeDto::EndTurn
                    },
                    stop_reason: Some("realtime_audio".into()),
                    cost: client::adapter::lowering::lower_cost_snapshot(
                        &orch.snapshot_cost_real().await,
                    ),
                })
                .await;
            }
            let mut active = active_cancel.lock().await;
            if active
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &task_turn))
            {
                *active = None;
            }
            drop(active);
            if let Some(owner_id) = task_turn.permission_owner_id {
                permission_gate.end_main_turn(owner_id);
            }
            message_queue.clear_active_turn().await;
            task_turn.mark_completed();
            let _ = result_tx.send(result);
        });
        turn.set_task_handle(task);
        drop(transition);
        result_rx.await.map_err(|_| {
            OrchestratorError::Internal("native audio owner task ended unexpectedly".into())
        })?
    }
}
