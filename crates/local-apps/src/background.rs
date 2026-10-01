//! Host-owned persistence for declarative local-app background tasks.

use crate::error::AppError;
use crate::manifest::AppLayout;
use crate::runtime_v2::{
    BackgroundJournalEntry, BackgroundTaskRecord, RUNTIME_CONTRACT_SCHEMA_VERSION,
};
use rooted_fs::AtomicWriteOptions;
use rooted_fs::FsError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const TASKS_FILE: &str = "background-tasks.json";
const JOURNAL_FILE: &str = "background-journal.json";
pub(crate) const CANCEL_DIR: &str = "background-cancel";
const MAX_CANCELLATION_BYTES: u64 = 64;
const MAX_BACKGROUND_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundTaskCatalog {
    schema_version: u32,
    tasks: Vec<BackgroundTaskRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundJournalCatalog {
    schema_version: u32,
    entries: Vec<BackgroundJournalEntry>,
}

fn path(layout: &AppLayout, file: &str) -> PathBuf {
    layout.app_dir_rel().join(file)
}

fn cancel_path(layout: &AppLayout, task_id: &str) -> Result<PathBuf, AppError> {
    if task_id.is_empty()
        || task_id.len() > 128
        || task_id
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return Err(AppError::InvalidRequest(
            "background task id contains unsupported characters".into(),
        ));
    }
    Ok(layout.app_dir_rel().join(CANCEL_DIR).join(task_id))
}

/// Request cancellation without waiting on the task's long-running process
/// lock. The running host observes this marker between steps and at terminal
/// persistence, then removes it. The app layout must already exist; this
/// function deliberately never creates an app directory, so a concurrent
/// delete cannot be undone by a cancellation request.
pub fn request_cancellation(layout: &AppLayout, task_id: &str) -> Result<(), AppError> {
    let marker = cancel_path(layout, task_id)?;
    rooted_fs::atomic_write(
        layout.root(),
        &marker,
        b"cancel\n",
        AtomicWriteOptions {
            create_parents: false,
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .map_err(|error| AppError::from_fs("write background cancellation", &error))
}

/// Check whether a task has a pending cancellation marker.
pub fn cancellation_requested(layout: &AppLayout, task_id: &str) -> Result<bool, AppError> {
    let marker = cancel_path(layout, task_id)?;
    match rooted_fs::read_to_string_limited(layout.root(), &marker, MAX_CANCELLATION_BYTES) {
        Ok(_) => Ok(true),
        Err(FsError::NotFound(_)) => Ok(false),
        Err(error) => Err(AppError::from_fs("read background cancellation", &error)),
    }
}

/// Remove a consumed cancellation marker. Missing markers are harmless.
pub fn clear_cancellation(layout: &AppLayout, task_id: &str) -> Result<(), AppError> {
    let marker = cancel_path(layout, task_id)?;
    match rooted_fs::remove_file(layout.root(), &marker) {
        Ok(()) | Err(FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(AppError::from_fs("remove background cancellation", &error)),
    }
}

/// Load the persisted per-app background task catalog.
pub fn load_tasks(layout: &AppLayout) -> Result<Vec<BackgroundTaskRecord>, AppError> {
    let body = match rooted_fs::read_to_string_limited(
        layout.root(),
        &path(layout, TASKS_FILE),
        MAX_BACKGROUND_BYTES,
    ) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(AppError::from_fs("read background task catalog", &error)),
    };
    let catalog: BackgroundTaskCatalog = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("background task catalog: {error}")))?;
    if catalog.schema_version != RUNTIME_CONTRACT_SCHEMA_VERSION {
        return Err(AppError::StorageCorrupt(format!(
            "background task catalog schemaVersion {} is unsupported",
            catalog.schema_version
        )));
    }
    for task in &catalog.tasks {
        if task.app_id != layout.app_id() || task.flow_id != task.flow.flow_id {
            return Err(AppError::StorageCorrupt(
                "background task ownership or flow id mismatch".into(),
            ));
        }
    }
    Ok(catalog.tasks)
}

/// Persist the complete per-app background task catalog atomically.
pub fn save_tasks(layout: &AppLayout, tasks: &[BackgroundTaskRecord]) -> Result<(), AppError> {
    if tasks
        .iter()
        .any(|task| task.app_id != layout.app_id() || task.flow_id != task.flow.flow_id)
    {
        return Err(AppError::InvalidRequest(
            "background task ownership or flow id mismatch".into(),
        ));
    }
    let mut body = serde_json::to_vec_pretty(&BackgroundTaskCatalog {
        schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
        tasks: tasks.to_vec(),
    })
    .map_err(|error| AppError::Io(format!("serialize background task catalog: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_BACKGROUND_BYTES {
        return Err(AppError::InvalidRequest(
            "background task catalog exceeds its size limit".into(),
        ));
    }
    layout.initialize()?;
    rooted_fs::atomic_write(
        layout.root(),
        &path(layout, TASKS_FILE),
        &body,
        AtomicWriteOptions {
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .map_err(|error| AppError::from_fs("write background task catalog", &error))
}

/// Load the resumable background execution journal for one app.
pub fn load_journal(layout: &AppLayout) -> Result<Vec<BackgroundJournalEntry>, AppError> {
    let body = match rooted_fs::read_to_string_limited(
        layout.root(),
        &path(layout, JOURNAL_FILE),
        MAX_BACKGROUND_BYTES,
    ) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(AppError::from_fs("read background journal", &error)),
    };
    let catalog: BackgroundJournalCatalog = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("background journal: {error}")))?;
    if catalog.schema_version != RUNTIME_CONTRACT_SCHEMA_VERSION {
        return Err(AppError::StorageCorrupt(
            "background journal schemaVersion is unsupported".into(),
        ));
    }
    Ok(catalog.entries)
}

/// Persist the complete per-app background execution journal atomically.
pub fn save_journal(
    layout: &AppLayout,
    entries: &[BackgroundJournalEntry],
) -> Result<(), AppError> {
    let mut body = serde_json::to_vec_pretty(&BackgroundJournalCatalog {
        schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
        entries: entries.to_vec(),
    })
    .map_err(|error| AppError::Io(format!("serialize background journal: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_BACKGROUND_BYTES {
        return Err(AppError::InvalidRequest(
            "background journal exceeds its size limit".into(),
        ));
    }
    layout.initialize()?;
    rooted_fs::atomic_write(
        layout.root(),
        &path(layout, JOURNAL_FILE),
        &body,
        AtomicWriteOptions {
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .map_err(|error| AppError::from_fs("write background journal", &error))
}

/// Persist the task catalog and journal with best-effort rollback if the
/// second atomic file write fails. The two files remain individually atomic,
/// while callers get a recoverable pair instead of a silently orphaned task.
pub fn save_state(
    layout: &AppLayout,
    tasks: &[BackgroundTaskRecord],
    entries: &[BackgroundJournalEntry],
) -> Result<(), AppError> {
    let previous_tasks = load_tasks(layout)?;
    save_tasks(layout, tasks)?;
    if let Err(error) = save_journal(layout, entries) {
        let rollback = save_tasks(layout, &previous_tasks);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(AppError::Io(format!(
                "persist background state failed: {error}; rollback failed: {rollback_error}"
            ))),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_v2::{
        BackgroundTaskStatus, BackgroundTrigger, CapabilityId, FlowDefinition, FlowStep,
    };
    use crate::storage::delete_app_dir;

    #[test]
    fn task_and_journal_round_trip_under_app_layout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(temp.path(), "abc12345").expect("layout");
        let flow = FlowDefinition {
            flow_id: "flow-1".into(),
            version: 1,
            steps: vec![FlowStep {
                step_id: "step-1".into(),
                capability: CapabilityId::RuntimeStatus,
                depends_on: Vec::new(),
                input_json: "{}".into(),
            }],
        };
        let task = BackgroundTaskRecord {
            schema_version: RUNTIME_CONTRACT_SCHEMA_VERSION,
            task_id: "task-1".into(),
            app_id: "abc12345".into(),
            flow_id: flow.flow_id.clone(),
            flow,
            trigger: BackgroundTrigger::Schedule {
                interval_ms: 900_000,
            },
            status: BackgroundTaskStatus::Scheduled,
            updated_at_ms: 1,
        };
        save_tasks(&layout, std::slice::from_ref(&task)).expect("save tasks");
        assert_eq!(load_tasks(&layout).expect("load tasks"), vec![task]);
        let journal = BackgroundJournalEntry {
            task_id: "task-1".into(),
            flow_id: "flow-1".into(),
            next_step_id: Some("step-1".into()),
            next_run_at_ms: Some(900_000),
            last_result_json: None,
            attempt: 1,
            last_error: None,
            updated_at_ms: 2,
        };
        save_journal(&layout, std::slice::from_ref(&journal)).expect("save journal");
        assert_eq!(load_journal(&layout).expect("load journal"), vec![journal]);
    }

    #[test]
    fn cancellation_marker_is_confined_and_consumable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(temp.path(), "abc12345").expect("layout");
        layout.initialize().expect("layout directories");

        request_cancellation(&layout, "task-1").expect("request cancellation");
        assert!(cancellation_requested(&layout, "task-1").expect("read marker"));
        clear_cancellation(&layout, "task-1").expect("clear marker");
        assert!(!cancellation_requested(&layout, "task-1").expect("read cleared marker"));
        assert!(request_cancellation(&layout, "../escape").is_err());
    }

    #[test]
    fn cancellation_does_not_recreate_deleted_app_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(temp.path(), "abc12345").expect("layout");
        layout.initialize().expect("layout directories");
        let app_dir = temp.path().join("apps/abc12345");

        delete_app_dir(temp.path(), "abc12345").expect("delete app");
        assert!(!app_dir.exists());
        assert!(request_cancellation(&layout, "task-1").is_err());
        assert!(!app_dir.exists());
    }
}
