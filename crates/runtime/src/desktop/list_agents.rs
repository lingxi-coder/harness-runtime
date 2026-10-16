//! Desktop producer for the local `/list-agents` roster.
//!
//! This reads the current runtime's ordered task/name/team stores. Current
//! workers are emitted first, followed by durable team-file members not already
//! represented by a live worker. Cross-session peer discovery is not projected;
//! successful snapshots retain the source warning that other local sessions
//! may be missing.

use command_api::builtins::list_agents::{
    ListAgentsCommandAvailability, ListAgentsProvider, LocalAgentRoster, LocalSubagentRow,
    LocalTeammateRow, LocalTeammateSince, TeammateNameShadowing, TeammateTimeKind,
};
use coordinator::{TeamRegistry, WorkerAgent};
use lingxi_core::host::agent_name_registry::AgentNameRegistry;
use lingxi_core::host::env::is_env_truthy;
use lingxi_core::host::task_registry::{
    FieldPresence, TaskListFilter, TaskRecord, TaskRegistryHandle,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const HARBOR_KITE_ENV: &str = "CLAUDE_CODE_HARBOR_KITE";
#[cfg(windows)]
const HARBOR_KITE_WINDOWS_FLAG: &str = "tengu_harbor_kite_win";
const HARBOR_KITE_FLAG: &str = "tengu_harbor_kite";
const LOCAL_AGENT_TASK: &str = "local_agent";
const IN_PROCESS_TEAMMATE_TASK: &str = "in_process_teammate";
const MAIN_SESSION_AGENT_TYPE: &str = "main-session";
const IN_PROCESS_PANE_MARKER: &str = "in-process";

/// Supplies the runtime-owned local state consumed by `/list-agents`.
///
/// The desktop provider API has no per-invocation teammate context. It is
/// assembled for the desktop session's current coordinator, so the exact self
/// identity comes from `TeamFile::lead_agent_id`; it does not infer a caller
/// from a worker name or coordinator UUID.
pub(crate) struct DesktopListAgentsProvider {
    tasks: Arc<dyn TaskRegistryHandle>,
    agent_names: Arc<dyn AgentNameRegistry>,
    team: Arc<TeamRegistry>,
    home: PathBuf,
}

impl DesktopListAgentsProvider {
    /// Bind the provider to the current desktop runtime's task, name, and team
    /// stores. `home` is the config home used by the coordinator team file.
    #[must_use]
    pub(crate) fn new(
        tasks: Arc<dyn TaskRegistryHandle>,
        agent_names: Arc<dyn AgentNameRegistry>,
        team: Arc<TeamRegistry>,
        home: PathBuf,
    ) -> Self {
        Self {
            tasks,
            agent_names,
            team,
            home,
        }
    }
}

#[async_trait::async_trait]
impl ListAgentsProvider for DesktopListAgentsProvider {
    fn command_availability(&self) -> ListAgentsCommandAvailability {
        ListAgentsCommandAvailability {
            harbor_kite_enabled: Some(harbor_kite_enabled()),
        }
    }

    async fn snapshot_local_roster(&self) -> Result<LocalAgentRoster, Utf16JsonProjection> {
        // TaskRegistryHandle::list preserves its ordered state snapshot when
        // called with the unfiltered default, matching Object.values order.
        let tasks = self
            .tasks
            .list(TaskListFilter::default())
            .await
            .map_err(|error| roster_error(format!("task registry read failed: {error}")))?;
        let named_agents = self.agent_names.list().await;
        let names_by_agent_id = reverse_agent_names(&named_agents);
        let workers = self.team.list().await;
        let team_worker_names: HashSet<String> =
            workers.iter().map(|worker| worker.name.clone()).collect();
        let subagents = local_subagent_rows(&tasks, &names_by_agent_id, &team_worker_names)?;

        let teammates = match self.team.team_name().await {
            Some(team_name) => match read_team_file(self.home.clone(), team_name.clone()).await? {
                Some(team_file) => teammates_from_sources(
                    &workers,
                    &tasks,
                    &team_file,
                    &team_name,
                    &named_agents
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>(),
                )?,
                // Native treats an absent team file as no durable roster. A
                // live worker still needs the file's pane marker in this host,
                // so do not invent an in-process/pane classification for it.
                None if workers.is_empty() => Vec::new(),
                None => return Err(roster_error("team file is missing for current workers")),
            },
            None if workers.is_empty() => Vec::new(),
            None => {
                return Err(roster_error(
                    "team workers exist but the coordinator has no team name",
                ));
            }
        };

        Ok(LocalAgentRoster {
            subagents,
            teammates,
            // The cross-session local peer query is not connected to this
            // runtime provider. Keep Native's warning rather than presenting
            // a local-only snapshot as a complete machine-wide listing.
            local_list_failed: true,
        })
    }
}

fn harbor_kite_enabled() -> bool {
    if let Some(raw) = std::env::var_os(HARBOR_KITE_ENV) {
        // Presence is an override, including a non-Unicode value (which has no
        // truthy JavaScript string representation and therefore evaluates
        // false). Do not fall through to GrowthBook when it is set.
        return is_env_truthy(raw.to_str());
    }

    #[cfg(windows)]
    {
        telemetry::feature_flags::flag_bool(HARBOR_KITE_WINDOWS_FLAG, true)
            && telemetry::feature_flags::flag_bool(HARBOR_KITE_FLAG, true)
    }
    #[cfg(not(windows))]
    {
        telemetry::feature_flags::flag_bool(HARBOR_KITE_FLAG, true)
    }
}

fn reverse_agent_names(
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

fn local_subagent_rows(
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

fn required_agent_string<'a>(
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

async fn read_team_file(
    home: PathBuf,
    team_name: String,
) -> Result<Option<coordinator::team_file::TeamFile>, Utf16JsonProjection> {
    tokio::task::spawn_blocking(move || coordinator::team_file::read_team_file(&home, &team_name))
        .await
        .map_err(|error| roster_error(format!("team file read task failed: {error}")))?
        .map(Some)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(roster_error(format!("team file read failed: {error}")))
            }
        })
}

fn teammates_from_sources(
    workers: &[WorkerAgent],
    tasks: &[TaskRecord],
    team_file: &coordinator::team_file::TeamFile,
    team_name: &str,
    agent_registry_names: &[&str],
) -> Result<Vec<LocalTeammateRow>, Utf16JsonProjection> {
    if team_file.name != team_name || team_file.lead_agent_id.is_empty() {
        return Err(roster_error(
            "team file is missing its current team identity",
        ));
    }

    let mut rows = Vec::with_capacity(workers.len() + team_file.members.len());
    let mut seen_agent_ids = HashSet::from([team_file.lead_agent_id.clone()]);

    for worker in workers {
        if worker.name.is_empty() || worker.agent_type.is_empty() || worker.task_id.is_empty() {
            return Err(roster_error(format!(
                "team worker {} is missing name, agent type, or task identity",
                worker.agent_id.as_uuid()
            )));
        }
        let advertised_id = format!("{}@{}", worker.name, team_name);
        if !teammate_name_is_addressable(&worker.name)
            || !seen_agent_ids.insert(advertised_id.clone())
        {
            continue;
        }

        let matching_tasks: Vec<_> = tasks
            .iter()
            .filter(|task| task.task_id == worker.task_id)
            .collect();
        let task = match matching_tasks.as_slice() {
            [task] => *task,
            [] => {
                return Err(roster_error(format!(
                    "team worker {} has no task record for {}",
                    worker.name, worker.task_id
                )));
            }
            _ => {
                return Err(roster_error(format!(
                    "team worker {} has duplicate task records for {}",
                    worker.name, worker.task_id
                )));
            }
        };
        if task.task_type != IN_PROCESS_TEAMMATE_TASK {
            return Err(roster_error(format!(
                "team worker {} is backed by unsupported task type {}",
                worker.name, task.task_type
            )));
        }
        if task.status.is_empty() {
            return Err(missing_task_fact(&task.task_id, "status"));
        }
        let facts = task
            .agent_facts
            .as_ref()
            .ok_or_else(|| missing_task_fact(&task.task_id, "agent_facts"))?;
        let stable_id =
            required_agent_string(&task.task_id, "stable_agent_id", &facts.stable_agent_id)?;
        let expected_stable_id = worker.agent_id.as_uuid().to_string();
        if stable_id != expected_stable_id.as_str() {
            return Err(roster_error(format!(
                "team worker {} does not match task {} identity",
                worker.name, task.task_id
            )));
        }
        let is_idle = match &facts.is_idle {
            FieldPresence::Value(value) => *value,
            FieldPresence::Missing | FieldPresence::Null => {
                return Err(missing_task_fact(&task.task_id, "is_idle"));
            }
        };

        let matching_members: Vec<_> = team_file
            .members
            .iter()
            .filter(|member| member.agent_id == advertised_id)
            .collect();
        let member = match matching_members.as_slice() {
            [member] => *member,
            [] => {
                return Err(roster_error(format!(
                    "team file has no member for {}",
                    advertised_id
                )));
            }
            _ => {
                return Err(roster_error(format!(
                    "team file has duplicate members for {}",
                    advertised_id
                )));
            }
        };
        if member.name != worker.name {
            return Err(roster_error(format!(
                "team file identity does not match current worker {}",
                worker.name
            )));
        }

        // The coordinator writes exactly `leader` for its own pane marker and
        // `in-process` for an in-process worker. Every other value is a real
        // pane id; empty or unknown values are not treated as a legacy fallback.
        let pane_backed = is_pane_backend(&member.tmux_pane_id);
        let since = system_time_ms(&worker.spawned_at).map(|timestamp_ms| LocalTeammateSince {
            kind: TeammateTimeKind::Started,
            timestamp_ms,
        });

        rows.push(LocalTeammateRow {
            name: Some(worker.name.clone().into()),
            agent_type: Some(worker.agent_type.clone().into()),
            status: (!pane_backed).then(|| {
                if is_idle {
                    "idle".into()
                } else {
                    task.status.clone().into()
                }
            }),
            backend: if pane_backed {
                "pane".into()
            } else {
                "in-process".into()
            },
            since,
            // Native's live team-context rows do not run the roster name
            // collision annotation; that check is reserved for file-only
            // members below.
            name_shadowing: TeammateNameShadowing::None,
        });
    }

    for member in &team_file.members {
        if seen_agent_ids.contains(&member.agent_id) {
            continue;
        }
        if member.agent_id.is_empty() || member.name.is_empty() {
            return Err(roster_error(
                "team file member is missing name or agent identity",
            ));
        }
        if !teammate_name_is_addressable(&member.name) {
            continue;
        }
        seen_agent_ids.insert(member.agent_id.clone());

        rows.push(LocalTeammateRow {
            name: Some(member.name.clone().into()),
            agent_type: member.agent_type.clone().map(Into::into),
            status: None,
            backend: "roster".into(),
            since: Some(LocalTeammateSince {
                kind: TeammateTimeKind::Joined,
                timestamp_ms: member.joined_at,
            }),
            name_shadowing: teammate_name_shadowing(&member.name, agent_registry_names),
        });
    }

    Ok(rows)
}

fn teammate_name_is_addressable(name: &str) -> bool {
    let normalized = agent::catalog::normalize_teammate_recipient(name);
    !matches!(
        normalized.as_str(),
        "main" | "team-lead" | "user" | "system"
    ) && !looks_like_agent_id(&normalized)
}

fn looks_like_agent_id(value: &str) -> bool {
    let Some(value) = value.strip_prefix('a') else {
        return false;
    };
    let (prefix, suffix) = match value.rsplit_once('-') {
        Some((prefix, suffix)) if !prefix.is_empty() => (Some(prefix), suffix),
        _ => (None, value),
    };
    suffix.len() == 16
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && prefix.is_none_or(|prefix| {
            (1..=63).contains(&prefix.len())
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
}

fn teammate_name_shadowing(name: &str, agent_registry_names: &[&str]) -> TeammateNameShadowing {
    let sanitized = lingxi_core::host::display::sanitize_display_to(name, usize::MAX)
        .chars()
        .take(200)
        .collect::<String>();
    let sanitized = if sanitized.is_empty() {
        "untitled session"
    } else {
        sanitized.trim()
    };
    let normalized = agent::catalog::normalize_teammate_recipient(sanitized);
    if !agent_registry_names
        .iter()
        .any(|name| agent::catalog::normalize_teammate_recipient(name) == normalized)
    {
        return TeammateNameShadowing::None;
    }

    if agent_registry_names.contains(&name) || sanitized != name {
        TeammateNameShadowing::Unreachable
    } else {
        TeammateNameShadowing::BareOnly
    }
}

fn is_pane_backend(tmux_pane_id: &str) -> bool {
    !matches!(tmux_pane_id, IN_PROCESS_PANE_MARKER | "leader")
}

fn system_time_ms(time: &SystemTime) -> Option<u64> {
    let millis = time.to_owned().duration_since(UNIX_EPOCH).ok()?.as_millis();
    u64::try_from(millis).ok()
}

fn missing_task_fact(task_id: &str, fact: &str) -> Utf16JsonProjection {
    roster_error(format!("task {task_id} is missing required {fact} fact"))
}

fn roster_error(message: impl Into<String>) -> Utf16JsonProjection {
    format!(
        "Could not read the current local agent roster: {}",
        message.into()
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::task_registry::AgentTaskFacts;
    use lingxi_core::types::AgentId;
    use std::time::Duration;

    fn local_agent(
        task_id: &str,
        agent_id: &AgentId,
        agent_type: &str,
        status: &str,
    ) -> TaskRecord {
        TaskRecord {
            task_id: task_id.into(),
            task_type: LOCAL_AGENT_TASK.into(),
            status: status.into(),
            started_at_ms: Some(1234),
            agent_facts: Some(AgentTaskFacts {
                stable_agent_id: FieldPresence::Value(agent_id.as_uuid().to_string()),
                agent_type: FieldPresence::Value(agent_type.into()),
                ..AgentTaskFacts::default()
            }),
            ..TaskRecord::default()
        }
    }

    #[test]
    fn local_subagents_keep_task_order_last_name_wins_and_team_names_are_suppressed() {
        let first_id = AgentId::new();
        let team_id = AgentId::new();
        let main_id = AgentId::new();
        let tasks = vec![
            local_agent("first", &first_id, "Explore", "running"),
            local_agent("team-shadow", &team_id, "Plan", "waiting"),
            local_agent("main", &main_id, MAIN_SESSION_AGENT_TYPE, "running"),
        ];
        let names = reverse_agent_names(&[
            ("old-name".into(), first_id.clone()),
            ("current-name".into(), first_id),
            ("worker".into(), team_id.clone()),
            ("lead".into(), main_id),
        ]);
        let team_names = HashSet::from(["worker".to_owned()]);

        let rows = local_subagent_rows(&tasks, &names, &team_names).expect("valid source facts");

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].name.as_ref().and_then(|name| name.string_at("")),
            Some("current-name")
        );
        assert_eq!(rows[0].status.string_at(""), Some("running"));
        assert_eq!(rows[0].started_at_ms, 1234);
        assert!(rows[1].name.is_none());
        assert_eq!(rows[1].status.string_at(""), Some("waiting"));
    }

    #[test]
    fn local_subagents_fail_when_required_source_fact_is_missing() {
        let agent_id = AgentId::new();
        let mut task = local_agent("missing-start", &agent_id, "Explore", "running");
        task.started_at_ms = None;

        let error = local_subagent_rows(&[task], &HashMap::new(), &HashSet::new())
            .expect_err("missing timestamp cannot become a successful partial roster");

        assert!(error
            .string_at("")
            .is_some_and(|message| message.contains("started_at_ms")));
    }

    #[test]
    fn team_backend_uses_only_current_source_markers() {
        assert!(!is_pane_backend("in-process"));
        assert!(!is_pane_backend("leader"));
        assert!(is_pane_backend("%7"));
        assert!(is_pane_backend(""));
    }

    fn team_member(
        agent_id: &str,
        name: &str,
        pane_id: &str,
        joined_at: u64,
    ) -> coordinator::team_file::TeamMember {
        coordinator::team_file::TeamMember {
            agent_id: agent_id.into(),
            name: name.into(),
            agent_type: Some("Explore".into()),
            model: None,
            model_profile: None,
            joined_at,
            tmux_pane_id: pane_id.into(),
            cwd: "/workspace".into(),
            subscriptions: Vec::new(),
        }
    }

    #[test]
    fn teammates_union_live_workers_then_file_only_members_and_exclude_exact_leader() {
        let worker_id = AgentId::new();
        let worker = WorkerAgent {
            awaiting_plan_approval: false,
            agent_id: worker_id.clone(),
            agent_type: "Explore".into(),
            name: "live-worker".into(),
            parent_id: None,
            status: coordinator::WorkerStatus::Working {
                activity: "reading".into(),
            },
            task_id: "task-live".into(),
            spawned_at: UNIX_EPOCH + Duration::from_millis(2_000),
            last_active_at: UNIX_EPOCH + Duration::from_millis(2_500),
        };
        let task = TaskRecord {
            task_id: "task-live".into(),
            task_type: IN_PROCESS_TEAMMATE_TASK.into(),
            status: "working".into(),
            agent_facts: Some(AgentTaskFacts {
                stable_agent_id: FieldPresence::Value(worker_id.as_uuid().to_string()),
                is_idle: FieldPresence::Value(false),
                ..AgentTaskFacts::default()
            }),
            ..TaskRecord::default()
        };
        let team_file = coordinator::team_file::TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: 1,
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            members: vec![
                team_member("team-lead@alpha", "team-lead", "leader", 10),
                team_member("live-worker@alpha", "live-worker", "%7", 20),
                team_member("file-worker@alpha", "Scout", "in-process", 30),
            ],
        };

        let rows = teammates_from_sources(
            &[worker],
            &[task],
            &team_file,
            "alpha",
            &["live-worker", "scout"],
        )
        .expect("current live and durable facts form a roster");

        assert_eq!(rows.len(), 2, "the exact leader row is not addressable");
        assert_eq!(
            rows[0].name.as_ref().and_then(|value| value.string_at("")),
            Some("live-worker")
        );
        assert_eq!(rows[0].backend.string_at(""), Some("pane"));
        assert_eq!(rows[0].status, None, "pane rows display their backend");
        assert_eq!(
            rows[0].since.as_ref().map(|since| since.kind),
            Some(TeammateTimeKind::Started)
        );
        assert_eq!(rows[0].name_shadowing, TeammateNameShadowing::None);

        assert_eq!(
            rows[1].name.as_ref().and_then(|value| value.string_at("")),
            Some("Scout")
        );
        assert_eq!(rows[1].backend.string_at(""), Some("roster"));
        assert!(rows[1].status.is_none());
        assert_eq!(
            rows[1].since.as_ref().map(|since| since.kind),
            Some(TeammateTimeKind::Joined)
        );
        assert_eq!(
            rows[1].since.as_ref().map(|since| since.timestamp_ms),
            Some(30)
        );
        assert_eq!(rows[1].name_shadowing, TeammateNameShadowing::BareOnly);
    }

    #[test]
    fn file_only_name_shadowing_distinguishes_exact_and_normalized_registry_names() {
        let mut file = coordinator::team_file::TeamFile {
            name: "alpha".into(),
            description: None,
            created_at: 1,
            lead_agent_id: "team-lead@alpha".into(),
            lead_session_id: None,
            members: vec![
                team_member("team-lead@alpha", "team-lead", "leader", 0),
                team_member("file@alpha", " Ｓｃｏｕｔ ", "in-process", 10),
            ],
        };

        let normalized_collision = teammates_from_sources(&[], &[], &file, "alpha", &["scout"])
            .expect("file-only row is valid");
        assert_eq!(normalized_collision.len(), 1);
        assert_eq!(
            normalized_collision[0].name_shadowing,
            TeammateNameShadowing::Unreachable,
            "NFKC and display cleanup make the source name collide"
        );

        file.members[1].name = "Scout".into();
        let exact_name = teammates_from_sources(&[], &[], &file, "alpha", &["Scout"])
            .expect("file-only row is valid");
        assert_eq!(
            exact_name[0].name_shadowing,
            TeammateNameShadowing::Unreachable
        );

        let unrelated = teammates_from_sources(&[], &[], &file, "alpha", &["Other"])
            .expect("file-only row is valid");
        assert_eq!(unrelated[0].name_shadowing, TeammateNameShadowing::None);
    }

    #[test]
    fn native_roster_name_filter_skips_reserved_and_agent_id_names() {
        for name in [
            "main",
            "team-lead",
            "user",
            "system",
            "a0123456789abcdef",
            "a-worker-0123456789abcdef",
        ] {
            assert!(!teammate_name_is_addressable(name), "{name}");
        }
        assert!(teammate_name_is_addressable("worker-one"));
    }
}
