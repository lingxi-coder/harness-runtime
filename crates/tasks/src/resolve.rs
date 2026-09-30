//! TaskStop target resolution (Claude Code 2.1.263 `XFe`, `_jn`, `Szo`).
use agent::catalog::normalize_teammate_recipient as normalize;
use lingxi_core::host::display::sanitize_display;
use lingxi_core::host::task_registry::{TaskRecord, TaskStopResolution};

/// Resolve a task id, teammate identity/name, or registered background name.
/// Exact spelling wins over normalized spelling; ambiguity never silently
/// chooses a target, even when every candidate has already completed.
pub(crate) fn resolve_stop_target(
    requested: &str,
    records: Vec<TaskRecord>,
    named_agents: &[(String, String)],
) -> TaskStopResolution {
    if let Some(record) = records.iter().find(|r| r.task_id == requested) {
        return TaskStopResolution::Found(record.clone());
    }
    for normalized in [false, true] {
        let key = if normalized {
            normalize(requested)
        } else {
            requested.into()
        };
        let matches = |name: &str| {
            if normalized {
                normalize(name) == key
            } else {
                name == key
            }
        };
        let exact_identity = (!normalized)
            .then(|| {
                records
                    .iter()
                    .filter(|r| r.teammate_agent_id.as_deref() == Some(requested))
                    .max_by_key(|r| r.status == "running")
            })
            .flatten();
        let mut teammates: Vec<&TaskRecord> = Vec::new();
        if let Some(record) = exact_identity {
            teammates.push(record);
        } else {
            for record in records.iter().filter(|r| {
                r.task_type == "in_process_teammate"
                    && r.teammate_name.as_deref().is_some_and(matches)
            }) {
                if let Some(existing) = teammates
                    .iter_mut()
                    .find(|r| r.teammate_agent_id == record.teammate_agent_id)
                {
                    if existing.status != "running" && record.status == "running" {
                        *existing = record;
                    }
                } else {
                    teammates.push(record);
                }
            }
            if teammates.iter().any(|r| r.status == "running") {
                teammates.retain(|r| r.status == "running");
            }
        }
        let named = named_agents
            .iter()
            .filter(|(name, _)| matches(name))
            .find_map(|(_, id)| {
                records
                    .iter()
                    .find(|r| r.task_type == "local_agent" && &r.task_id == id)
            });
        let ids = teammates
            .iter()
            .filter_map(|r| r.teammate_agent_id.as_deref())
            .map(sanitize_display)
            .collect::<Vec<_>>()
            .join(", ");
        let shown = sanitize_display(requested);
        if let Some(named) = named {
            if !teammates.is_empty() {
                return TaskStopResolution::Ambiguous(format!(
                    "\"{shown}\" matches both teammate {ids} and background agent {}. Use the full agent ID (name@team) for the teammate or the task ID for the background agent.", sanitize_display(&named.task_id)));
            }
            return TaskStopResolution::Found(named.clone());
        }
        if teammates.len() > 1 {
            return TaskStopResolution::Ambiguous(format!(
                "Multiple teammates match \"{shown}\": {ids}. Use the full agent ID (name@team)."
            ));
        }
        if let Some(record) = teammates.first() {
            return TaskStopResolution::Found((*record).clone());
        }
    }
    let mut candidates: Vec<(String, String)> = Vec::new();
    for record in records
        .iter()
        .filter(|r| r.task_type == "in_process_teammate" && r.status == "running")
    {
        if let (Some(name), Some(id)) = (&record.teammate_name, &record.teammate_agent_id) {
            upsert(&mut candidates, normalize(name), id.clone());
        }
    }
    for (name, id) in named_agents {
        if records.iter().any(|r| {
            &r.task_id == id
                && r.task_type == "local_agent"
                && (r.status == "running" || r.is_parked)
        }) {
            upsert(&mut candidates, normalize(name), name.clone());
        }
    }
    let key = normalize(requested);
    let suggestion = candidates
        .into_iter()
        .filter_map(|(name, display)| {
            let distance = edit_distance(&key, &name);
            (distance <= 2).then_some((distance, display))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, display)| display);
    TaskStopResolution::NotFound { suggestion }
}

fn upsert(candidates: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some(entry) = candidates.iter_mut().find(|(name, _)| name == &key) {
        entry.1 = value;
    } else {
        candidates.push((key, value));
    }
}

/// `Kne`: adjacent transposition counts as one edit, using JS UTF-16 units.
fn edit_distance(left: &str, right: &str) -> usize {
    let a: Vec<_> = left.encode_utf16().collect();
    let b: Vec<_> = right.encode_utf16().collect();
    if a.len().abs_diff(b.len()) > 2 {
        return 3;
    }
    let mut rows = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        rows[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            rows[i][j] = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]));
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                rows[i][j] = rows[i][j].min(rows[i - 2][j - 2] + 1);
            }
        }
    }
    rows[a.len()][b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn teammate(id: &str, name: &str, status: &str) -> TaskRecord {
        TaskRecord {
            task_id: format!("task-{id}"),
            task_type: "in_process_teammate".into(),
            teammate_agent_id: Some(id.into()),
            teammate_name: Some(name.into()),
            status: status.into(),
            ..Default::default()
        }
    }
    fn found(result: TaskStopResolution) -> String {
        match result {
            TaskStopResolution::Found(r) => r.task_id,
            other => panic!("expected found: {other:?}"),
        }
    }
    #[test]
    fn resolves_id_exact_names_nfkc_and_prefers_live_teammates() {
        let rows = vec![
            teammate("bob@old", "Bob", "completed"),
            teammate("bob@new", "Bob", "running"),
        ];
        assert_eq!(
            found(resolve_stop_target("bob@old", rows.clone(), &[])),
            "task-bob@old"
        );
        assert_eq!(
            found(resolve_stop_target("Bob", rows.clone(), &[])),
            "task-bob@new"
        );
        assert_eq!(
            found(resolve_stop_target(" ＢＯＢ ", rows, &[])),
            "task-bob@new"
        );
    }
    #[test]
    fn ambiguity_never_stops_a_teammate_or_background_namesake() {
        let mut rows = vec![
            teammate("bob@one", "Bob", "running"),
            teammate("bob@two", "Bob", "running"),
        ];
        assert!(
            matches!(resolve_stop_target("Bob", rows.clone(), &[]), TaskStopResolution::Ambiguous(s) if s.contains("Multiple teammates"))
        );
        rows.truncate(1);
        rows.push(TaskRecord {
            task_id: "agent123".into(),
            task_type: "local_agent".into(),
            ..Default::default()
        });
        assert!(
            matches!(resolve_stop_target("Bob", rows, &[("Bob".into(), "agent123".into())]), TaskStopResolution::Ambiguous(s) if s.contains("matches both teammate"))
        );
    }
    #[test]
    fn typo_suggestion_includes_parked_named_agents_and_transpositions() {
        let rows = vec![TaskRecord {
            task_id: "agent123".into(),
            task_type: "local_agent".into(),
            status: "completed".into(),
            is_parked: true,
            ..Default::default()
        }];
        assert!(
            matches!(resolve_stop_target("alhpa", rows, &[("alpha".into(), "agent123".into())]), TaskStopResolution::NotFound {suggestion: Some(s)} if s == "alpha")
        );
        assert_eq!(edit_distance("alhpa", "alpha"), 1);
        assert_eq!(edit_distance("zzzzzz", "alpha"), 6);
    }
}
