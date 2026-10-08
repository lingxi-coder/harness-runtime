//! Native-shaped reduction of the live `$.agent.list()` task snapshot.
//!
//! The input must be composed from the ordered task registry, the live ordered
//! agent-name map, and a team-membership snapshot. This module does not read
//! those stores itself; it preserves source facts and reports missing host
//! producers separately from the Native JSON result.

use crate::host::task_registry::{AgentTaskFacts, FieldPresence, TaskRecord};
use crate::types::AgentId;
use serde::ser::{SerializeStruct, Serializer};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Inputs captured by the composition root for one list call.
///
/// `task_rows` stays in `TaskRows::ordered_states` / `Object.values` order.
/// The name list has the same insertion order as JavaScript `Map`; replacing a
/// name does not move it. `None` for team membership means the host could not
/// provide the membership snapshot, while `Some(empty)` is a successful empty
/// snapshot.
pub struct AgentListSnapshot<'a> {
    pub task_rows: &'a [TaskRecord],
    pub name_entries: &'a [(String, AgentId)],
    pub inactive_teammate_addresses: Option<&'a HashSet<String>>,
}

/// A parity limitation in the host snapshot. These diagnostics are not part of
/// the Native list JSON; callers can report them without inventing facts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentListUnresolvedFact {
    pub task_id: String,
    pub field: &'static str,
}

/// Ordered Native list entries plus any missing host-source facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentListReduction {
    pub entries: Vec<AgentListEntry>,
    pub unresolved_facts: Vec<AgentListUnresolvedFact>,
}

/// One `$.agent.list()` item. The custom serializer preserves Native field
/// order and JavaScript conditional-spread behavior: `Missing` is omitted,
/// explicit `Null` is emitted as JSON null.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentListEntry {
    pub id: FieldPresence<Value>,
    pub teammate_id: FieldPresence<Value>,
    pub description: FieldPresence<Value>,
    pub agent_type: FieldPresence<Value>,
    pub status: FieldPresence<Value>,
    pub parent_id: FieldPresence<Value>,
    pub spawned_by: FieldPresence<Value>,
    pub name: FieldPresence<Value>,
}

impl Serialize for AgentListEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let field_count = [
            is_present(&self.id),
            is_present(&self.teammate_id),
            is_present(&self.description),
            is_present(&self.agent_type),
            is_present(&self.status),
            is_present(&self.parent_id),
            is_present(&self.spawned_by),
            is_present(&self.name),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        let mut row = serializer.serialize_struct("AgentListEntry", field_count)?;
        serialize_presence(&mut row, "id", &self.id)?;
        serialize_presence(&mut row, "teammateId", &self.teammate_id)?;
        serialize_presence(&mut row, "description", &self.description)?;
        serialize_presence(&mut row, "type", &self.agent_type)?;
        serialize_presence(&mut row, "status", &self.status)?;
        serialize_presence(&mut row, "parentId", &self.parent_id)?;
        serialize_presence(&mut row, "spawnedBy", &self.spawned_by)?;
        serialize_presence(&mut row, "name", &self.name)?;
        row.end()
    }
}

fn is_present<T>(presence: &FieldPresence<T>) -> bool {
    !matches!(presence, FieldPresence::Missing)
}

fn serialize_presence<S>(
    row: &mut S,
    name: &'static str,
    presence: &FieldPresence<Value>,
) -> Result<(), S::Error>
where
    S: SerializeStruct,
{
    match presence {
        FieldPresence::Missing => Ok(()),
        FieldPresence::Null => row.serialize_field(name, &Value::Null),
        FieldPresence::Value(value) => row.serialize_field(name, value),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MapKey {
    Undefined,
    Json(String),
}

fn map_key(id: &FieldPresence<Value>) -> MapKey {
    match id {
        FieldPresence::Missing => MapKey::Undefined,
        FieldPresence::Null => MapKey::Json("null".into()),
        FieldPresence::Value(value) => MapKey::Json(
            serde_json::to_string(value).expect("JSON value serialization is infallible"),
        ),
    }
}

fn field_value<T>(field: &FieldPresence<T>, map: impl FnOnce(&T) -> Value) -> FieldPresence<Value> {
    match field {
        FieldPresence::Missing => FieldPresence::Missing,
        FieldPresence::Null => FieldPresence::Null,
        FieldPresence::Value(value) => FieldPresence::Value(map(value)),
    }
}

fn string_value(value: impl Into<String>) -> FieldPresence<Value> {
    FieldPresence::Value(Value::String(value.into()))
}

fn presence_string(value: &FieldPresence<String>) -> FieldPresence<Value> {
    field_value(value, |value| Value::String(value.clone()))
}

fn nullish_json(value: &FieldPresence<Value>, fallback: Option<&Value>) -> FieldPresence<Value> {
    match value {
        FieldPresence::Value(Value::Null) | FieldPresence::Null | FieldPresence::Missing => {
            fallback.map_or(FieldPresence::Missing, |value| {
                FieldPresence::Value(value.clone())
            })
        }
        FieldPresence::Value(value) => FieldPresence::Value(value.clone()),
    }
}

fn optional_presence_value(presence: &FieldPresence<Value>) -> Option<&Value> {
    match presence {
        FieldPresence::Missing => None,
        FieldPresence::Null => Some(&Value::Null),
        FieldPresence::Value(value) => Some(value),
    }
}

fn is_true(value: &FieldPresence<bool>) -> bool {
    matches!(value, FieldPresence::Value(true))
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed")
}

fn is_agent_row(task_type: &str) -> bool {
    matches!(task_type, "local_agent" | "in_process_teammate")
}

fn stable_id(record: &TaskRecord, facts: &AgentTaskFacts) -> FieldPresence<Value> {
    match record.task_type.as_str() {
        "local_agent" => presence_string(&facts.stable_agent_id),
        "in_process_teammate" => match &facts.resumable_agent_id {
            FieldPresence::Value(value) => string_value(value.clone()),
            FieldPresence::Null | FieldPresence::Missing => facts.teammate_id.clone(),
        },
        _ => FieldPresence::Missing,
    }
}

fn fact_gaps(
    record: &TaskRecord,
    facts: Option<&AgentTaskFacts>,
    team_snapshot_available: bool,
) -> Vec<AgentListUnresolvedFact> {
    let mut gaps = Vec::new();
    let Some(facts) = facts else {
        gaps.push(AgentListUnresolvedFact {
            task_id: record.task_id.clone(),
            field: "agent_facts",
        });
        return gaps;
    };
    match record.task_type.as_str() {
        "local_agent" if matches!(record.status.as_str(), "running" | "completed") => {
            for (field, missing) in [
                ("is_idle", matches!(&facts.is_idle, FieldPresence::Missing)),
                (
                    "finalizing",
                    matches!(&facts.finalizing, FieldPresence::Missing),
                ),
                (
                    "keepalive_reasons",
                    matches!(&facts.keepalive_reasons, FieldPresence::Missing),
                ),
            ] {
                if missing {
                    gaps.push(AgentListUnresolvedFact {
                        task_id: record.task_id.clone(),
                        field,
                    });
                }
            }
        }
        "in_process_teammate"
            if record.status == "running"
                && matches!(&facts.resumable_agent_id, FieldPresence::Missing)
                && !team_snapshot_available =>
        {
            gaps.push(AgentListUnresolvedFact {
                task_id: record.task_id.clone(),
                field: "team_membership",
            });
        }
        _ => {}
    }
    gaps
}

/// Reduce a source-ordered task snapshot to Native list rows.
///
/// This implements `h_e -> PAt -> MAt -> idle-only GTe`, then the
/// source-order-sensitive duplicate rule from `ifo`. It never sorts by stable
/// ID. The returned diagnostics identify missing host producers separately.
pub fn reduce_agent_list(snapshot: AgentListSnapshot<'_>) -> AgentListReduction {
    let mut names_by_stable_id = HashMap::<String, String>::new();
    for (name, agent_id) in snapshot.name_entries {
        names_by_stable_id.insert(agent_id.as_uuid().to_string(), name.clone());
    }

    let mut active_activity = HashSet::<String>::new();
    for record in snapshot.task_rows {
        if record.status != "running" {
            continue;
        }
        let Some(facts) = record.agent_facts.as_ref() else {
            continue;
        };
        let FieldPresence::Value(agent_id) = &facts.activity_agent_id else {
            continue;
        };
        let active = match record.task_type.as_str() {
            "local_bash" => is_true(&facts.is_backgrounded),
            "monitor_mcp" | "monitor_ws" => true,
            _ => false,
        };
        if active {
            active_activity.insert(agent_id.clone());
        }
    }

    let membership_available = snapshot.inactive_teammate_addresses.is_some();
    let inactive_teammate_addresses = snapshot
        .inactive_teammate_addresses
        .cloned()
        .unwrap_or_default();
    let mut entries = Vec::<AgentListEntry>::new();
    let mut index_by_id = HashMap::<MapKey, usize>::new();
    let mut saw_nonterminal = HashSet::<MapKey>::new();
    let mut unresolved_facts = HashSet::<AgentListUnresolvedFact>::new();

    for record in snapshot.task_rows {
        let facts = record.agent_facts.as_ref();
        unresolved_facts.extend(fact_gaps(record, facts, membership_available));
        if !is_agent_row(&record.task_type) {
            continue;
        }
        let Some(facts) = facts else {
            continue;
        };

        let id = stable_id(record, facts);
        let key = map_key(&id);
        let terminal = is_terminal(&record.status);
        if terminal && saw_nonterminal.contains(&key) {
            continue;
        }
        if !terminal {
            saw_nonterminal.insert(key.clone());
        }

        let stable_string = match &id {
            FieldPresence::Value(Value::String(value)) => Some(value.as_str()),
            _ => None,
        };
        let registry_name = stable_string.and_then(|id| names_by_stable_id.get(id));
        let registry_name_value = registry_name.map(|name| Value::String(name.clone()));
        let row_name = nullish_json(&facts.name, registry_name_value.as_ref());
        let has_name = is_present(&row_name);

        let mut status = native_status(record, facts, has_name);
        if record.task_type == "in_process_teammate"
            && record.status == "running"
            && matches!(&facts.resumable_agent_id, FieldPresence::Missing)
        {
            if let FieldPresence::Value(Value::String(teammate_id)) = &facts.teammate_id {
                if inactive_teammate_addresses.contains(teammate_id) {
                    status = "idle".into();
                }
            }
        }
        if status == "idle" && stable_string.is_some_and(|id| active_activity.contains(id)) {
            status = "waiting".into();
        }

        let description = if record.task_type == "in_process_teammate" {
            let spawned_description = presence_string(&facts.spawned_description);
            nullish_json(
                &spawned_description,
                optional_presence_value(&facts.row_description),
            )
        } else {
            facts.row_description.clone()
        };
        let agent_type = match record.task_type.as_str() {
            "local_agent" => presence_string(&facts.agent_type),
            "in_process_teammate" => match &facts.agent_type {
                FieldPresence::Value(value) if !value.is_empty() => string_value(value.clone()),
                _ => string_value("teammate"),
            },
            _ => FieldPresence::Missing,
        };
        let entry = AgentListEntry {
            id,
            teammate_id: if record.task_type == "in_process_teammate" {
                facts.teammate_id.clone()
            } else {
                FieldPresence::Missing
            },
            description,
            agent_type,
            status: string_value(status),
            parent_id: facts.parent_id.clone(),
            spawned_by: facts.spawned_by.clone(),
            name: row_name,
        };
        if let Some(index) = index_by_id.get(&key).copied() {
            entries[index] = entry;
        } else {
            index_by_id.insert(key, entries.len());
            entries.push(entry);
        }
    }

    let mut unresolved_facts = unresolved_facts.into_iter().collect::<Vec<_>>();
    unresolved_facts.sort_by(|left, right| {
        left.task_id
            .cmp(&right.task_id)
            .then_with(|| left.field.cmp(right.field))
    });
    AgentListReduction {
        entries,
        unresolved_facts,
    }
}

fn native_status(record: &TaskRecord, facts: &AgentTaskFacts, has_name: bool) -> String {
    if record.status == "paused" {
        return "idle".into();
    }
    match record.task_type.as_str() {
        "local_agent" if record.status == "running" => {
            if is_true(&facts.is_idle) {
                "waiting".into()
            } else {
                record.status.clone()
            }
        }
        "in_process_teammate" if record.status == "running" => {
            if is_true(&facts.awaiting_plan_approval) {
                "waiting".into()
            } else if is_true(&facts.is_idle) {
                "idle".into()
            } else {
                record.status.clone()
            }
        }
        "local_agent" if record.status == "completed" => {
            if is_true(&facts.finalizing) {
                return "running".into();
            }
            match &facts.keepalive_reasons {
                FieldPresence::Value(reasons) if !reasons.is_empty() => {
                    if reasons.iter().any(|reason| reason != "flag:idle-window") {
                        "waiting".into()
                    } else {
                        "idle".into()
                    }
                }
                _ if has_name => "idle".into(),
                _ => record.status.clone(),
            }
        }
        _ => record.status.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fact<T>(value: T) -> FieldPresence<T> {
        FieldPresence::Value(value)
    }

    fn local(id: &str, status: &str, description: &str) -> TaskRecord {
        TaskRecord {
            task_id: format!("task-{id}-{status}-{description}"),
            task_type: "local_agent".into(),
            status: status.into(),
            description: description.into(),
            agent_facts: Some(AgentTaskFacts {
                stable_agent_id: fact(id.into()),
                row_description: fact(json!(description)),
                agent_type: fact("Explore".into()),
                is_idle: fact(false),
                finalizing: fact(false),
                keepalive_reasons: fact(Vec::new()),
                ..AgentTaskFacts::default()
            }),
            ..TaskRecord::default()
        }
    }

    fn teammate(id: &str, address: FieldPresence<Value>, status: &str) -> TaskRecord {
        TaskRecord {
            task_id: format!("task-teammate-{id}-{status}"),
            task_type: "in_process_teammate".into(),
            status: status.into(),
            description: "row description".into(),
            agent_facts: Some(AgentTaskFacts {
                teammate_id: address,
                resumable_agent_id: fact(id.into()),
                row_description: fact(json!("row description")),
                spawned_description: fact("spawned description".into()),
                agent_type: fact("Explore".into()),
                ..AgentTaskFacts::default()
            }),
            ..TaskRecord::default()
        }
    }

    fn reduce(rows: &[TaskRecord], names: &[(String, AgentId)]) -> AgentListReduction {
        reduce_agent_list(AgentListSnapshot {
            task_rows: rows,
            name_entries: names,
            inactive_teammate_addresses: Some(&HashSet::new()),
        })
    }

    #[test]
    fn preserves_native_duplicate_precedence_order_and_output_fields() {
        let raw_a = "00000000-0000-0000-0000-00000000000a";
        let raw_b = "00000000-0000-0000-0000-00000000000b";
        let raw_d = "00000000-0000-0000-0000-00000000000d";
        let raw_id = |value: &str| AgentId::from_uuid(uuid::Uuid::parse_str(value).unwrap());
        let agent_a = raw_id(raw_a);
        let agent_b = raw_id(raw_b);
        let a_first = local(raw_a, "running", "A first active");
        let b_first = local(raw_b, "running", "B first-seen");
        let mut a_last = local(raw_a, "running", "A last active");
        a_last.agent_facts.as_mut().unwrap().parent_id = fact(json!("parent-A"));
        a_last.agent_facts.as_mut().unwrap().spawned_by = fact(json!("spawn-plugin"));
        let mut a_terminal = local(raw_a, "completed", "A later terminal");
        a_terminal.agent_facts.as_mut().unwrap().keepalive_reasons = fact(Vec::new());
        let d_first = local(raw_d, "completed", "D first terminal");
        let d_last = local(raw_d, "failed", "D last terminal");
        let reduction = reduce(
            &[
                a_first.clone(),
                b_first,
                a_last,
                a_terminal,
                d_first,
                d_last,
            ],
            &[
                ("B mapped name".into(), agent_b),
                ("A name".into(), agent_a),
            ],
        );
        assert_eq!(
            serde_json::to_string(&reduction.entries).unwrap(),
            r#"[{"id":"00000000-0000-0000-0000-00000000000a","description":"A last active","type":"Explore","status":"running","parentId":"parent-A","spawnedBy":"spawn-plugin","name":"A name"},{"id":"00000000-0000-0000-0000-00000000000b","description":"B first-seen","type":"Explore","status":"running","name":"B mapped name"},{"id":"00000000-0000-0000-0000-00000000000d","description":"D last terminal","type":"Explore","status":"failed"}]"#
        );

        // Terminal first, then active, then terminal: the active wins, while
        // Map insertion order remains the first occurrence of the id.
        let paused = local("agent-C", "paused", "paused nonterminal");
        let terminal = local("agent-C", "failed", "later terminal");
        let before = local("agent-C", "completed", "old terminal");
        let reduction = reduce(&[before, paused, terminal], &[]);
        assert_eq!(reduction.entries.len(), 1);
        assert_eq!(
            reduction.entries[0].description,
            fact(json!("paused nonterminal"))
        );
        assert_eq!(reduction.entries[0].status, fact(json!("idle")));
    }

    #[test]
    fn applies_native_status_priority_membership_and_activity_promotion() {
        let mut idle_local = local("local", "running", "local");
        idle_local.agent_facts.as_mut().unwrap().is_idle = fact(true);
        let mut plan_teammate = teammate("loop", fact(json!("plan@team")), "running");
        plan_teammate
            .agent_facts
            .as_mut()
            .unwrap()
            .awaiting_plan_approval = fact(true);
        plan_teammate
            .agent_facts
            .as_mut()
            .unwrap()
            .resumable_agent_id = FieldPresence::Missing;
        let mut idle_teammate = teammate("loop2", fact(json!("idle@team")), "running");
        idle_teammate.agent_facts.as_mut().unwrap().is_idle = fact(true);

        let mut finalizing = local("fin", "completed", "finalizing");
        finalizing.agent_facts.as_mut().unwrap().finalizing = fact(true);
        let mut keepalive = local("child", "completed", "keepalive");
        keepalive.agent_facts.as_mut().unwrap().keepalive_reasons =
            fact(vec!["owned-child".into()]);
        let mut idle_window = local("window", "completed", "idle window");
        idle_window.agent_facts.as_mut().unwrap().keepalive_reasons =
            fact(vec!["flag:idle-window".into()]);
        let paused = local("paused", "paused", "paused");
        let unknown_status = local("unknown", "custom-status", "unknown");

        let mut inactive = teammate("none", fact(json!("worker@team")), "running");
        inactive.agent_facts.as_mut().unwrap().resumable_agent_id = FieldPresence::Missing;
        let inactive_with_bash = TaskRecord {
            task_id: "bash".into(),
            task_type: "local_bash".into(),
            status: "running".into(),
            agent_facts: Some(AgentTaskFacts {
                activity_agent_id: fact("worker@team".into()),
                is_backgrounded: fact(true),
                ..AgentTaskFacts::default()
            }),
            ..TaskRecord::default()
        };
        let mut inactive_with_monitor = teammate("none2", fact(json!("mon@team")), "running");
        inactive_with_monitor
            .agent_facts
            .as_mut()
            .unwrap()
            .resumable_agent_id = FieldPresence::Missing;
        let monitor = TaskRecord {
            task_id: "monitor".into(),
            task_type: "monitor_mcp".into(),
            status: "running".into(),
            agent_facts: Some(AgentTaskFacts {
                activity_agent_id: fact("mon@team".into()),
                ..AgentTaskFacts::default()
            }),
            ..TaskRecord::default()
        };

        let rows = [
            idle_local,
            plan_teammate,
            idle_teammate,
            finalizing,
            keepalive,
            idle_window,
            paused,
            unknown_status,
            inactive,
            inactive_with_bash,
            inactive_with_monitor,
            monitor,
        ];
        let inactive_addresses = HashSet::from([
            "plan@team".to_owned(),
            "worker@team".to_owned(),
            "mon@team".to_owned(),
        ]);
        let result = reduce_agent_list(AgentListSnapshot {
            task_rows: &rows,
            name_entries: &[],
            inactive_teammate_addresses: Some(&inactive_addresses),
        });
        let statuses = result
            .entries
            .iter()
            .map(|entry| entry.status.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                fact(json!("waiting")), // local isIdle
                fact(json!("idle")),    // MAt overrides plan waiting
                fact(json!("idle")),    // teammate isIdle
                fact(json!("running")), // finalizing wins
                fact(json!("waiting")), // non-idle-window keepalive
                fact(json!("idle")),    // idle-window keepalive
                fact(json!("idle")),    // paused
                fact(json!("custom-status")),
                fact(json!("waiting")), // background Bash GTe
                fact(json!("waiting")), // monitor GTe
            ]
        );

        let mut resumable = teammate("loop", fact(json!("resumable@team")), "running");
        resumable.agent_facts.as_mut().unwrap().is_idle = fact(false);
        let result = reduce_agent_list(AgentListSnapshot {
            task_rows: &[resumable],
            name_entries: &[],
            inactive_teammate_addresses: Some(&inactive_addresses),
        });
        assert_eq!(result.entries[0].status, fact(json!("running")));

        let mut explicit_null = teammate("loop", fact(json!("null-resumable@team")), "running");
        explicit_null
            .agent_facts
            .as_mut()
            .unwrap()
            .resumable_agent_id = FieldPresence::Null;
        let null_membership = HashSet::from(["null-resumable@team".to_owned()]);
        let result = reduce_agent_list(AgentListSnapshot {
            task_rows: &[explicit_null],
            name_entries: &[],
            inactive_teammate_addresses: Some(&null_membership),
        });
        assert_eq!(
            result.entries[0].status,
            fact(json!("running")),
            "Native MAt tests strict undefined; explicit null skips team-file override"
        );
    }

    #[test]
    fn preserves_missing_null_empty_and_raw_agent_id_semantics() {
        let mut null_teammate = teammate("stable-null-address", FieldPresence::Null, "running");
        let facts = null_teammate.agent_facts.as_mut().unwrap();
        facts.resumable_agent_id = fact("stable-null-address".into());
        facts.spawned_description = FieldPresence::Null;
        facts.parent_id = FieldPresence::Null;
        facts.spawned_by = FieldPresence::Null;
        facts.name = fact(json!("Teammate"));
        let mut missing_address =
            teammate("stable-missing-address", FieldPresence::Missing, "running");
        let facts = missing_address.agent_facts.as_mut().unwrap();
        facts.row_description = FieldPresence::Missing;
        facts.spawned_description = FieldPresence::Missing;
        facts.agent_type = FieldPresence::Missing;
        facts.name = fact(json!(""));
        let names = [(
            "".into(),
            AgentId::from_uuid(
                uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            ),
        )];
        let reduction = reduce(&[null_teammate, missing_address], &names);
        assert_eq!(
            serde_json::to_string(&reduction.entries).unwrap(),
            r#"[{"id":"stable-null-address","teammateId":null,"description":"row description","type":"Explore","status":"running","parentId":null,"spawnedBy":null,"name":"Teammate"},{"id":"stable-missing-address","type":"teammate","status":"running","name":""}]"#
        );

        let raw_id = AgentId::from_uuid(
            uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        );
        let local_row = local(&raw_id.as_uuid().to_string(), "running", "raw-id");
        let reduction = reduce(&[local_row], &[("registered local name".into(), raw_id)]);
        assert_eq!(
            reduction.entries[0].id,
            fact(json!(raw_id.as_uuid().to_string()))
        );
        assert_eq!(
            reduction.entries[0].name,
            fact(json!("registered local name"))
        );

        let duplicate_name_row = local(&raw_id.as_uuid().to_string(), "running", "duplicate name");
        let duplicate_names = reduce(
            &[duplicate_name_row],
            &[("first name".into(), raw_id), ("last name".into(), raw_id)],
        );
        assert_eq!(
            duplicate_names.entries[0].name,
            fact(json!("last name")),
            "the reverse name map is filled in Native Map iteration order"
        );
    }

    #[test]
    fn reports_unproduced_status_facts_instead_of_claiming_a_complete_snapshot() {
        let mut row = local("id", "completed", "description");
        let facts = row.agent_facts.as_mut().unwrap();
        facts.is_idle = FieldPresence::Missing;
        facts.finalizing = FieldPresence::Missing;
        facts.keepalive_reasons = FieldPresence::Missing;
        let result = reduce(&[row], &[]);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            result.unresolved_facts,
            vec![
                AgentListUnresolvedFact {
                    task_id: "task-id-completed-description".into(),
                    field: "finalizing"
                },
                AgentListUnresolvedFact {
                    task_id: "task-id-completed-description".into(),
                    field: "is_idle"
                },
                AgentListUnresolvedFact {
                    task_id: "task-id-completed-description".into(),
                    field: "keepalive_reasons"
                },
            ]
        );
    }
}
