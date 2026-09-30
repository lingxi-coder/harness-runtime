//! Orchestrator-side [`TodoReminderTaskProvider`] backed by the file-backed V2
//! [`tool_task::todo_store::TodoStore`].
//!
//! Finding #73: the V2 (`task_reminder`) variant of the per-turn todo reminder
//! lists the existing tasks (`B4p` reading `p9(KF())`). This adapter supplies
//! that list to [`ConversationOrchestrator::todo_reminder_message`] by resolving
//! the active task-list id and reading the store, mirroring the binary's `KF()`
//! precedence (`utils/tasks.ts`):
//!
//! 1. `LINGXI_TASK_LIST_ID` env (explicit override);
//! 2. in-process teammate `teamName` — N/A at the orchestrator seam (no tool
//!    `ToolUseContext` here), so skipped;
//! 3. `LINGXI_TEAM_NAME` env (`zp()`, process-based teammate);
//! 4. leader team name ([`lingxi_core::host::team_registry::leader_team_name_for_session`]);
//! 5. live orchestrator session id fallback.
//!
//! This is the SAME resolution the V2 `Task*` tools use ([`tool_task`]
//! `resolve_task_list_id`) for levels 1/3/4 and now also level 5, so the
//! reminder reads the SAME on-disk task dir the `Task*` tools use in both
//! team/env-driven and standalone sessions. V1 (`todo_reminder`, the
//! default-when-tasks-disabled path) needs no provider and is unaffected.

use async_trait::async_trait;

use crate::prompt::todo_reminder::{TaskReminderItem, TodoReminderTaskProvider};

/// [`TodoReminderTaskProvider`] that reads the file-backed V2 task store for the
/// active list each turn.
pub struct TodoStoreReminderTasks {
    config_home: Option<std::path::PathBuf>,
}

impl TodoStoreReminderTasks {
    /// Construct the store-backed provider. Stateless — it resolves the active
    /// list id at call time so a session initialization / env change is
    /// honored.
    #[must_use]
    pub fn new() -> Self {
        Self { config_home: None }
    }

    /// Pin reminder reads to a host-resolved config home.
    #[must_use]
    pub fn with_config_home(config_home: std::path::PathBuf) -> Self {
        Self {
            config_home: Some(config_home),
        }
    }

    /// Resolve the active task-list id via the `KF()` precedence, using the
    /// live orchestrator session as the standalone fallback.
    fn resolve_list_id(session_id: lingxi_core::types::SessionId) -> String {
        // 1. Explicit env override.
        if let Some(explicit) = std::env::var_os("LINGXI_TASK_LIST_ID") {
            if !explicit.is_empty() {
                return explicit.to_string_lossy().into_owned();
            }
        }
        // 3. `LINGXI_TEAM_NAME` env (process-based teammate; TS `getTeamName()`).
        if let Some(team) = std::env::var_os("LINGXI_TEAM_NAME") {
            if !team.is_empty() {
                return team.to_string_lossy().into_owned();
            }
        }
        // 4. Leader team name belonging to this session.
        if let Some(team) =
            lingxi_core::host::team_registry::leader_team_name_for_session(&session_id.to_string())
                .filter(|t| !t.is_empty())
        {
            return team;
        }
        // 5. Standalone session fallback.
        session_id.to_string()
    }
}

impl Default for TodoStoreReminderTasks {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TodoReminderTaskProvider for TodoStoreReminderTasks {
    async fn task_items(&self, session_id: lingxi_core::types::SessionId) -> Vec<TaskReminderItem> {
        let list_id = Self::resolve_list_id(session_id);
        let store = self.config_home.as_ref().map_or_else(
            || tool_task::todo_store::TodoStore::for_list(&list_id),
            |home| tool_task::todo_store::TodoStore::for_list_at(home, &list_id),
        );
        store
            .list()
            .await
            .into_iter()
            .map(|t| TaskReminderItem {
                id: t.id,
                status: t.status,
                subject: t.subject,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::TodoState;
    use tool_task::todo_store::{TodoStore, TodoTask};

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn session_fallback_reads_standalone_store() {
        let _g = ENV_LOCK.lock().await;
        let prev = std::env::var_os("LINGXI_TASK_LIST_ID");
        let prev_team = std::env::var_os("LINGXI_TEAM_NAME");
        let prev_config_dir = std::env::var_os(branding::CONFIG_DIR_ENV);
        std::env::remove_var("LINGXI_TASK_LIST_ID");
        std::env::remove_var("LINGXI_TEAM_NAME");

        let temp = tempfile::tempdir().expect("tempdir");
        std::env::set_var(branding::CONFIG_DIR_ENV, temp.path());
        let session_id = lingxi_core::types::SessionId::new();
        let store = TodoStore::for_list(&session_id.to_string());
        let id = store
            .create(TodoTask::new(
                "ship fix".into(),
                "wire reminder fallback".into(),
                None,
                serde_json::Map::new(),
            ))
            .await
            .expect("task created");

        let items = TodoStoreReminderTasks::new().task_items(session_id).await;
        assert_eq!(
            items,
            vec![TaskReminderItem {
                id,
                status: TodoState::Pending,
                subject: "ship fix".into(),
            }]
        );

        if let Some(v) = prev {
            std::env::set_var("LINGXI_TASK_LIST_ID", v);
        } else {
            std::env::remove_var("LINGXI_TASK_LIST_ID");
        }
        if let Some(v) = prev_team {
            std::env::set_var("LINGXI_TEAM_NAME", v);
        } else {
            std::env::remove_var("LINGXI_TEAM_NAME");
        }
        if let Some(v) = prev_config_dir {
            std::env::set_var(branding::CONFIG_DIR_ENV, v);
        } else {
            std::env::remove_var(branding::CONFIG_DIR_ENV);
        }
    }

    #[tokio::test]
    async fn explicit_env_preserves_precedence_over_session_fallback() {
        let _g = ENV_LOCK.lock().await;
        let prev = std::env::var_os("LINGXI_TASK_LIST_ID");
        let prev_team = std::env::var_os("LINGXI_TEAM_NAME");
        std::env::set_var("LINGXI_TASK_LIST_ID", "explicit-list");
        std::env::set_var("LINGXI_TEAM_NAME", "env-team");
        let session_id = lingxi_core::types::SessionId::new();
        lingxi_core::host::team_registry::set_leader_team_name_for_session(
            &session_id.to_string(),
            Some("leader-team"),
        );

        assert_eq!(
            TodoStoreReminderTasks::resolve_list_id(session_id),
            "explicit-list"
        );

        std::env::remove_var("LINGXI_TASK_LIST_ID");
        assert_eq!(
            TodoStoreReminderTasks::resolve_list_id(session_id),
            "env-team"
        );
        std::env::remove_var("LINGXI_TEAM_NAME");
        assert_eq!(
            TodoStoreReminderTasks::resolve_list_id(session_id),
            "leader-team"
        );
        let other = lingxi_core::types::SessionId::new();
        lingxi_core::host::team_registry::set_leader_team_name_for_session(
            &other.to_string(),
            Some("other-team"),
        );
        assert_eq!(TodoStoreReminderTasks::resolve_list_id(other), "other-team");
        assert_eq!(
            TodoStoreReminderTasks::resolve_list_id(session_id),
            "leader-team"
        );
        lingxi_core::host::team_registry::set_leader_team_name_for_session(
            &other.to_string(),
            None,
        );
        lingxi_core::host::team_registry::set_leader_team_name_for_session(
            &session_id.to_string(),
            None,
        );
        assert_eq!(
            TodoStoreReminderTasks::resolve_list_id(session_id),
            session_id.to_string()
        );

        if let Some(v) = prev {
            std::env::set_var("LINGXI_TASK_LIST_ID", v);
        } else {
            std::env::remove_var("LINGXI_TASK_LIST_ID");
        }
        if let Some(v) = prev_team {
            std::env::set_var("LINGXI_TEAM_NAME", v);
        } else {
            std::env::remove_var("LINGXI_TEAM_NAME");
        }
    }
}
