//! Host-owned persistence for local-app runtime profile migration metadata.

use crate::error::AppError;
use crate::ids;
use crate::manifest::{AppLayout, AppRuntimeProfileBinding};
use crate::types::APPS_SCHEMA_VERSION;
use rooted_fs::AtomicWriteOptions;
use rooted_fs::FsError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const JOURNAL_FILE: &str = "runtime-profile-migration.json";
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;

/// One declared runtime-profile migration path within a fixed family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProfileMigrationEdge {
    /// Runtime family this migration belongs to.
    pub family: crate::types::AppRuntimeProfile,
    /// Source catalog revision.
    pub from_revision: u32,
    /// Target catalog revision.
    pub to_revision: u32,
    /// Whether the target contract is rebuild-compatible with existing app
    /// code, or only a future code-aware migrator may use it.
    pub rebuild_compatible: bool,
}

impl RuntimeProfileMigrationEdge {
    /// Validate one declared migration edge.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.from_revision == 0 || self.to_revision == 0 {
            return Err(AppError::InvalidRequest(
                "runtime profile migration edge revisions must be at least 1".into(),
            ));
        }
        if self.to_revision <= self.from_revision {
            return Err(AppError::InvalidRequest(
                "runtime profile migration edge must increase the revision".into(),
            ));
        }
        Ok(())
    }
}

/// First release ships no runtime-profile migration edges.
pub const RUNTIME_PROFILE_MIGRATION_EDGES: &[RuntimeProfileMigrationEdge] = &[];

/// Durable status of one in-progress or completed runtime-profile migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeProfileMigrationStatus {
    /// Journal was created but work has not started yet.
    Pending,
    /// Host is actively applying the migration.
    Running,
    /// Migration completed successfully.
    Succeeded,
    /// Migration failed and the original state remains authoritative.
    Failed,
    /// Migration attempt rolled back after a partial failure.
    RolledBack,
}

/// Durable journal record for one runtime-profile migration attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProfileMigrationJournal {
    /// Persisted schema version ([`APPS_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// App this journal belongs to.
    pub app_id: String,
    /// Bound runtime profile before the migration.
    pub from: AppRuntimeProfileBinding,
    /// Bound runtime profile the host is attempting to apply.
    pub to: AppRuntimeProfileBinding,
    /// Current durable status.
    pub status: RuntimeProfileMigrationStatus,
    /// Creation time, epoch milliseconds.
    pub created_at_ms: u64,
    /// Last update time, epoch milliseconds.
    pub updated_at_ms: u64,
    /// Terminal timestamp, present only for finished states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// Last terminal or step error, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl RuntimeProfileMigrationJournal {
    fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != APPS_SCHEMA_VERSION {
            return Err(AppError::InvalidRequest(format!(
                "runtime profile migration journal schemaVersion {} is unsupported (expected {APPS_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        ids::validate_app_id(&self.app_id)?;
        self.from.validate()?;
        self.to.validate()?;
        if self.from.family != self.to.family {
            return Err(AppError::InvalidRequest(
                "runtime profile migration journal must stay within one family".into(),
            ));
        }
        if self.to.revision <= self.from.revision {
            return Err(AppError::InvalidRequest(
                "runtime profile migration journal must increase the revision".into(),
            ));
        }
        if self.updated_at_ms < self.created_at_ms {
            return Err(AppError::InvalidRequest(
                "runtime profile migration journal updatedAtMs must be >= createdAtMs".into(),
            ));
        }
        let is_terminal = matches!(
            self.status,
            RuntimeProfileMigrationStatus::Succeeded
                | RuntimeProfileMigrationStatus::Failed
                | RuntimeProfileMigrationStatus::RolledBack
        );
        match (is_terminal, self.finished_at_ms) {
            (true, Some(finished_at_ms)) if finished_at_ms >= self.updated_at_ms => {}
            (true, Some(_)) => {
                return Err(AppError::InvalidRequest(
                    "runtime profile migration journal finishedAtMs must be >= updatedAtMs".into(),
                ));
            }
            (true, None) => {
                return Err(AppError::InvalidRequest(
                    "runtime profile migration journal terminal states require finishedAtMs".into(),
                ));
            }
            (false, None) => {}
            (false, Some(_)) => {
                return Err(AppError::InvalidRequest(
                    "runtime profile migration journal non-terminal states cannot set finishedAtMs"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}

fn journal_path(layout: &AppLayout) -> PathBuf {
    layout.app_dir_rel().join(JOURNAL_FILE)
}

/// Load the per-app runtime-profile migration journal.
pub fn load_runtime_profile_migration_journal(
    layout: &AppLayout,
) -> Result<Option<RuntimeProfileMigrationJournal>, AppError> {
    let body = match rooted_fs::read_to_string_limited(
        layout.root(),
        &journal_path(layout),
        MAX_JOURNAL_BYTES,
    ) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(None),
        Err(error) => {
            return Err(AppError::from_fs(
                "read runtime profile migration journal",
                &error,
            ))
        }
    };
    let journal: RuntimeProfileMigrationJournal = serde_json::from_str(&body).map_err(|error| {
        AppError::StorageCorrupt(format!("runtime profile migration journal: {error}"))
    })?;
    journal.validate().map_err(|error| {
        AppError::StorageCorrupt(format!(
            "invalid runtime profile migration journal: {error}"
        ))
    })?;
    if journal.app_id != layout.app_id() {
        return Err(AppError::StorageCorrupt(format!(
            "runtime profile migration journal app id {:?} does not match directory {:?}",
            journal.app_id,
            layout.app_id()
        )));
    }
    Ok(Some(journal))
}

/// Persist the per-app runtime-profile migration journal atomically.
pub fn save_runtime_profile_migration_journal(
    layout: &AppLayout,
    journal: &RuntimeProfileMigrationJournal,
) -> Result<(), AppError> {
    journal.validate()?;
    if journal.app_id != layout.app_id() {
        return Err(AppError::InvalidRequest(format!(
            "runtime profile migration journal app id {:?} does not match layout app id {:?}",
            journal.app_id,
            layout.app_id()
        )));
    }
    let mut body = serde_json::to_vec_pretty(journal).map_err(|error| {
        AppError::Io(format!(
            "serialize runtime profile migration journal: {error}"
        ))
    })?;
    body.push(b'\n');
    if body.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(AppError::InvalidRequest(
            "runtime profile migration journal exceeds its size limit".into(),
        ));
    }
    layout.initialize()?;
    rooted_fs::atomic_write(
        layout.root(),
        &journal_path(layout),
        &body,
        AtomicWriteOptions {
            file_mode: 0o600,
            ..AtomicWriteOptions::default()
        },
    )
    .map_err(|error| AppError::from_fs("write runtime profile migration journal", &error))
}

/// Remove the per-app runtime-profile migration journal. Missing files are harmless.
pub fn delete_runtime_profile_migration_journal(layout: &AppLayout) -> Result<(), AppError> {
    match rooted_fs::remove_file(layout.root(), &journal_path(layout)) {
        Ok(()) | Err(FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(AppError::from_fs(
            "remove runtime profile migration journal",
            &error,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(
        family: crate::types::AppRuntimeProfile,
        revision: u32,
        hex: char,
    ) -> AppRuntimeProfileBinding {
        AppRuntimeProfileBinding {
            family,
            revision,
            contract_sha256: std::iter::repeat_n(hex, 64).collect(),
        }
    }

    fn sample_journal(app_id: &str) -> RuntimeProfileMigrationJournal {
        RuntimeProfileMigrationJournal {
            schema_version: APPS_SCHEMA_VERSION,
            app_id: app_id.into(),
            from: binding(crate::types::AppRuntimeProfile::Canvas2d, 1, 'a'),
            to: binding(crate::types::AppRuntimeProfile::Canvas2d, 2, 'b'),
            status: RuntimeProfileMigrationStatus::Running,
            created_at_ms: 10,
            updated_at_ms: 20,
            finished_at_ms: None,
            last_error: None,
        }
    }

    #[test]
    fn migration_edges_start_empty() {
        assert!(RUNTIME_PROFILE_MIGRATION_EDGES.is_empty());
    }

    #[test]
    fn journal_round_trips_under_app_layout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(temp.path(), "abc12345").expect("layout");
        let journal = sample_journal("abc12345");
        save_runtime_profile_migration_journal(&layout, &journal).expect("save journal");
        assert_eq!(
            load_runtime_profile_migration_journal(&layout).expect("load journal"),
            Some(journal)
        );
    }

    #[test]
    fn journal_delete_is_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(temp.path(), "abc12345").expect("layout");
        let journal = sample_journal("abc12345");
        save_runtime_profile_migration_journal(&layout, &journal).expect("save journal");
        delete_runtime_profile_migration_journal(&layout).expect("delete once");
        delete_runtime_profile_migration_journal(&layout).expect("delete twice");
        assert_eq!(
            load_runtime_profile_migration_journal(&layout).expect("load journal"),
            None
        );
    }

    #[test]
    fn journal_rejects_cross_family_or_non_advancing_revisions() {
        let mut mismatch = sample_journal("abc12345");
        mismatch.to.family = crate::types::AppRuntimeProfile::Three3d;
        assert!(mismatch.validate().is_err());

        let mut same_revision = sample_journal("abc12345");
        same_revision.to.revision = same_revision.from.revision;
        assert!(same_revision.validate().is_err());
    }

    #[test]
    fn save_rejects_foreign_app_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(temp.path(), "abc12345").expect("layout");
        let journal = sample_journal("other999");
        let error = save_runtime_profile_migration_journal(&layout, &journal)
            .expect_err("foreign app id must be rejected");
        assert_eq!(error.code(), crate::error::AppErrorCode::InvalidRequest);
        assert!(error.to_string().contains("does not match layout app id"));
    }

    #[test]
    fn load_rejects_unsupported_schema_version() {
        let temp = tempfile::tempdir().expect("tempdir");
        let layout = AppLayout::new(temp.path(), "abc12345").expect("layout");
        layout.initialize().expect("initialize");
        let mut journal = sample_journal("abc12345");
        journal.schema_version = APPS_SCHEMA_VERSION - 1;
        let mut body = serde_json::to_vec_pretty(&journal).expect("serialize");
        body.push(b'\n');
        rooted_fs::atomic_write(
            layout.root(),
            &journal_path(&layout),
            &body,
            AtomicWriteOptions {
                file_mode: 0o600,
                ..AtomicWriteOptions::default()
            },
        )
        .expect("seed journal");

        let error = load_runtime_profile_migration_journal(&layout)
            .expect_err("old schema journal must be rejected");
        assert_eq!(error.code(), crate::error::AppErrorCode::StorageCorrupt);
        assert!(error.to_string().contains(&format!(
            "schemaVersion {} is unsupported",
            APPS_SCHEMA_VERSION - 1
        )));
    }

    #[test]
    fn edge_validation_requires_advancing_revisions() {
        assert!(RuntimeProfileMigrationEdge {
            family: crate::types::AppRuntimeProfile::ReactDom,
            from_revision: 1,
            to_revision: 2,
            rebuild_compatible: true,
        }
        .validate()
        .is_ok());
        assert!(RuntimeProfileMigrationEdge {
            family: crate::types::AppRuntimeProfile::ReactDom,
            from_revision: 2,
            to_revision: 2,
            rebuild_compatible: true,
        }
        .validate()
        .is_err());
    }
}
