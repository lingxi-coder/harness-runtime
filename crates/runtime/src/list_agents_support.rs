//! Shared current-task projection for Desktop and Mobile `/list-agents`.
//!
//! Keep this fact conversion independent of either host's team/coordinator
//! services so both consumers validate the same task identity, ordering, and
//! required source fields.

use command_api::builtins::list_agents::LocalSubagentRow;
use lingxi_core::host::task_registry::{FieldPresence, TaskRecord};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::{HashMap, HashSet};

const LOCAL_AGENT_TASK: &str = "local_agent";
const MAIN_SESSION_AGENT_TYPE: &str = "main-session";

pub(crate) fn reverse_agent_names(
    entries: &[(String, lingxi_core::types::AgentId)],
) -> HashMap<String, String> {
    let mut names_by_id = HashMap::with_capacity(entries.len());
    for (name, agent_id) in entries {
        // The source is an insertion-ordered Map. A later name for the same
        // agent replaces the earlier reverse-map value without reordering the
        // task rows.
        names_by_id.insert(agent_id.as_uuid().to_string(), name.clone());
    }
    names_by_id
}

pub(crate) fn local_subagent_rows(
    tasks: &[TaskRecord],
    names_by_agent_id: &HashMap<String, String>,
    team_worker_names: &HashSet<String>,
) -> Result<Vec<LocalSubagentRow>, Utf16JsonProjection> {
    let mut rows = Vec::new();

    for task in tasks
        .iter()
        .filter(|task| task.task_type == LOCAL_AGENT_TASK)
    {
        let facts = task
            .agent_facts
            .as_ref()
            .ok_or_else(|| missing_task_fact(&task.task_id, "agent_facts"))?;
        let stable_id =
            required_agent_string(&task.task_id, "stable_agent_id", &facts.stable_agent_id)?;
        let agent_type = required_agent_string(&task.task_id, "agent_type", &facts.agent_type)?;
        let started_at_ms = task
            .started_at_ms
            .ok_or_else(|| missing_task_fact(&task.task_id, "started_at_ms"))?;
        if task.status.is_empty() {
            return Err(missing_task_fact(&task.task_id, "status"));
        }

        if agent_type == MAIN_SESSION_AGENT_TYPE {
            continue;
        }

        let source_name = names_by_agent_id.get(stable_id).cloned();
        let name = source_name
            .filter(|name| !team_worker_names.contains(name))
            .map(Utf16JsonProjection::from);

        rows.push(LocalSubagentRow {
            name,
            agent_type: agent_type.into(),
            status: task.status.clone().into(),
            started_at_ms,
        });
    }

    Ok(rows)
}

pub(crate) fn required_agent_string<'a>(
    task_id: &str,
    field: &'static str,
    presence: &'a FieldPresence<String>,
) -> Result<&'a str, Utf16JsonProjection> {
    match presence {
        FieldPresence::Value(value) if !value.is_empty() => Ok(value),
        FieldPresence::Value(_) | FieldPresence::Missing | FieldPresence::Null => {
            Err(missing_task_fact(task_id, field))
        }
    }
}

pub(crate) fn missing_task_fact(task_id: &str, fact: &str) -> Utf16JsonProjection {
    format!("Could not read the current local agent roster: task {task_id} is missing required {fact} fact")
        .into()
}
