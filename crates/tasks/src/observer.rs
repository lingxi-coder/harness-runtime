//! Independent observer task creation and non-interrupting activity delivery.

use crate::id::TaskType;
use crate::registry::TaskRegistry;
use crate::state::{TaskState, TaskStatus};
use crate::task_trait::{TaskContext, TaskError, TaskSpawnInput};
use agent::observer_text as agent_observer_text;
use lingxi_core::host::{SubagentInheritance, SubagentSpawnRequest};
use lingxi_core::types::AgentId;

impl TaskRegistry {
    /// File the observed↔observer pairing so `ObserverReport` can resolve a
    /// destination (oracle `Pme`, reduced to what this spawn path knows).
    ///
    /// The report target is the distinction that matters. An observer paired
    /// straight to an agent reports to THAT agent. An observer of a
    /// coordinator's worker reports to the COORDINATOR — the worker is the
    /// thing being watched, not the audience — so the pairing records the
    /// coordinator as the target and the worker as `via_worker_name`, which is
    /// what makes the brief tell the observer to name the worker in its report.
    fn arm_observer_pairing(
        &self,
        seed: Option<&lingxi_core::host::observer_pairing::ObserverPairingSeed>,
        observed_agent_id: AgentId,
        observer_task_id: AgentId,
    ) {
        let (Some(table), Some(seed)) = (self.observer_pairings(), seed) else {
            return;
        };
        arm_pairing(table, seed, observed_agent_id, observer_task_id);
    }
}

/// See [`TaskRegistry::arm_observer_pairing`]. Free so the report-target rule
/// is testable without standing up a whole registry.
pub(crate) fn arm_pairing(
    table: &lingxi_core::host::observer_pairing::ObserverPairings,
    seed: &lingxi_core::host::observer_pairing::ObserverPairingSeed,
    observed_agent_id: AgentId,
    observer_task_id: AgentId,
) {
    {
        let spec = &seed.spec;
        let envelope = agent_observer_text::envelope_name(&seed.observed_name);
        let mut pairing = lingxi_core::host::observer_pairing::ObserverPairing::armed(
            observer_task_id,
            spec,
            envelope.clone(),
            envelope.clone(),
        );
        pairing.observed_task_id = Some(observed_agent_id);
        match seed.observed_creator {
            // A worker spawned BY a coordinator: the report goes up to the
            // coordinator, and the brief must name the worker.
            Some(coordinator) => {
                pairing.report_target_task_id = Some(coordinator);
                pairing.report_target_name = seed
                    .observed_creator_name
                    .clone()
                    .unwrap_or_else(|| coordinator.to_string());
                pairing.via_worker_name = Some(envelope);
            }
            // Paired straight to the observed agent.
            None => {
                pairing.report_target_task_id = Some(observed_agent_id);
            }
        }
        table.insert(observed_agent_id.to_string(), pairing);
    }
}

impl TaskRegistry {
    pub(crate) async fn deliver_observer_digest(
        &self,
        task_id: &str,
        digest: String,
    ) -> Result<(), TaskError> {
        let handler = self
            .handler_for(TaskType::LocalAgent)
            .await
            .ok_or(TaskError::UnknownType)?;
        handler
            .send_message(
                task_id,
                digest,
                TaskContext {
                    fs: self.fs.clone(),
                    runtime: self.runtime.clone(),
                },
            )
            .await
    }

    pub(crate) async fn observe_agent_activity(
        &self,
        mut request: SubagentSpawnRequest,
        inheritance: SubagentInheritance,
        observed_agent_id: AgentId,
        digest: String,
        seed: Option<lingxi_core::host::observer_pairing::ObserverPairingSeed>,
    ) -> Result<(), TaskError> {
        // Pair lookup and first publication form one transaction, including
        // concurrent lifecycle taps restored for the same observed agent.
        let _pairing = self.observer_activity_lock.lock().await;
        let resume = {
            let mut tasks = self.tasks.write().await;
            let existing = tasks.values_mut().find_map(|state| match state {
                TaskState::LocalAgent(agent)
                    if agent.is_observer
                        && agent.observed_agent_id == Some(observed_agent_id)
                        && agent.subagent_type == request.subagent_type =>
                {
                    Some(agent)
                }
                _ => None,
            });
            if let Some(agent) = existing {
                if agent.base.status.is_terminal() && !agent.is_parked {
                    // An explicitly stopped observer is never silently relaunched.
                    return Err(TaskError::TerminatedTask);
                }
                agent.pending_messages.push(digest.clone());
                if !agent.is_parked {
                    return Ok(());
                }
                agent.is_parked = false;
                agent.base.status = TaskStatus::Running;
                Some((
                    agent.base.id.clone(),
                    std::mem::take(&mut agent.pending_messages).join("\n\n"),
                ))
            } else {
                None
            }
        };
        if let Some((id, digest)) = resume {
            return self.deliver_observer_digest(&id, digest).await;
        }
        // The pairing comes from `seed`, never from `request`. By the time this
        // runs `request` is the OBSERVER's — the caller cleared the declaration
        // and the observed agent's name out of it and rewrote `description` to
        // "<observer>@<observed>". Deriving the pairing from it armed nothing
        // and named the envelope after the wrong agent.
        let observer_task_id = AgentId::new();
        self.arm_observer_pairing(seed.as_ref(), observed_agent_id, observer_task_id);
        request.observer = None;
        request.run_in_background = true;
        request.query_source_label =
            Some(lingxi_core::host::subagent_spawn::OBSERVER_QUERY_SOURCE.into());
        request.creator_agent_id = None;
        request.prompt = format!("{}\n\n{}", request.prompt, digest);
        let description = request
            .description
            .clone()
            .unwrap_or_else(|| format!("Observer {}", request.subagent_type));
        let input = TaskSpawnInput::LocalAgent {
            agent_id: observer_task_id,
            subagent_type: request.subagent_type.clone(),
            prompt: request.prompt.clone(),
            is_backgrounded: true,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: Some(observed_agent_id),
            spawn_request: Some(request),
            inheritance: Some(inheritance),
        };
        self.spawn(TaskType::LocalAgent, input, description).await?;
        Ok(())
    }
}

#[cfg(test)]
mod pairing_tests {
    use super::arm_pairing;
    use lingxi_core::host::observer_pairing::{ObserverPairingSeed, ObserverPairings};
    use lingxi_core::host::subagent_spawn::ObserverSpec;
    use lingxi_core::types::AgentId;

    fn seed(creator: Option<AgentId>) -> ObserverPairingSeed {
        ObserverPairingSeed {
            spec: ObserverSpec::new("reviewer"),
            observed_name: "step two".into(),
            observed_creator: creator,
            observed_creator_name: creator.map(|_| "coordinator".to_string()),
        }
    }

    /// A pairing filed at spawn must be resolvable by the tool that reports
    /// through it — that is the whole point of sharing one table.
    #[test]
    fn arming_files_a_pairing_the_report_tool_can_resolve() {
        let table = ObserverPairings::new();
        let observed = AgentId::new();
        let observer = AgentId::new();
        arm_pairing(&table, &seed(None), observed, observer);
        let found = table
            .armed_for_observer(&observer)
            .expect("ObserverReport must resolve the pairing just armed");
        assert_eq!(found.observed_task_id, Some(observed));
        assert_eq!(found.report_target_task_id, Some(observed));
        assert!(found.via_worker_name.is_none());
        // the envelope name is slugged, not the raw display name
        assert_eq!(found.observed_envelope_name, "step-two");
    }

    /// The report goes UP to the coordinator, not to the worker being watched.
    /// Backwards, the observer would report into the very task it is supposed
    /// to be watching from outside.
    #[test]
    fn a_coordinators_worker_reports_to_the_coordinator_and_names_the_worker() {
        let table = ObserverPairings::new();
        let observed = AgentId::new();
        let observer = AgentId::new();
        let coordinator = AgentId::new();
        arm_pairing(&table, &seed(Some(coordinator)), observed, observer);
        let found = table.armed_for_observer(&observer).expect("armed");
        assert_eq!(found.report_target_task_id, Some(coordinator));
        assert_ne!(found.report_target_task_id, Some(observed));
        assert_eq!(found.report_target_name, "coordinator");
        assert_eq!(found.via_worker_name.as_deref(), Some("step-two"));
    }
}
