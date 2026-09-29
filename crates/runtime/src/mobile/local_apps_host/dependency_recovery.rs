use super::dependency_integrity::clone_or_copy_tree;
use super::DependencyUpdateFileBackup;
use super::LocalAppsHostBroker;
use super::WORKSPACE_DEPENDENCY_ATTESTATION_FILE;
use local_apps::AppLayout;
use local_apps::AppService;
use serde::Deserialize;
use serde::Serialize;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) const DEPENDENCY_UPDATE_RECOVERY_FILE_REL: &str =
    ".lingxi-build-state/dependency-update-recovery.json";

pub(super) const DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION: u32 = 1;

pub(super) const MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct DependencyUpdateRollback {
    pub(super) previous_dependency: local_apps::AppDependencyRecord,
    pub(super) files: Vec<DependencyUpdateFileBackup>,
    pub(super) manifest_bytes: Vec<u8>,
    pub(super) node_modules_backup: Option<PathBuf>,
    pub(super) build_backup: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DependencyUpdateRecoveryStatus {
    InProgress,
    Committed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DependencyUpdateRecoveryFile {
    pub(super) relative: String,
    pub(super) bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DependencyUpdateRecoveryJournal {
    pub(super) schema_version: u32,
    pub(super) app_id: String,
    pub(super) status: DependencyUpdateRecoveryStatus,
    pub(super) previous_dependency: local_apps::AppDependencyRecord,
    pub(super) files: Vec<DependencyUpdateRecoveryFile>,
    pub(super) manifest_bytes: Vec<u8>,
    pub(super) node_modules_backup: Option<String>,
    pub(super) build_backup: Option<String>,
}

impl LocalAppsHostBroker {
    pub(super) fn capture_dependency_update_rollback(
        &self,
        layout: &AppLayout,
        previous_dependency: local_apps::AppDependencyRecord,
    ) -> Result<DependencyUpdateRollback, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let mut files = Vec::new();
        for relative in [
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL,
            crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::TREE_PROOF_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SNAPSHOT_FILE_REL,
            "package.json",
            "pnpm-lock.yaml",
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
        ] {
            let path = workspace.join(relative);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!(
                        "read dependency rollback source {}: {error}",
                        path.display()
                    ))
                }
            };
            files.push(DependencyUpdateFileBackup { relative, bytes });
        }
        let manifest_path = layout.root().join(layout.manifest_rel());
        let manifest_bytes = std::fs::read(&manifest_path).map_err(|error| {
            format!(
                "read dependency rollback manifest {}: {error}",
                manifest_path.display()
            )
        })?;
        let node_modules = workspace.join("node_modules");
        let node_modules_backup = match std::fs::symlink_metadata(&node_modules) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err("workspace node_modules must not be a symlink".into());
                }
                if metadata.is_dir() {
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default();
                    let backup = workspace
                        .join(".lingxi-build-state")
                        .join(format!("dependency-update-rollback-node_modules-{stamp}"));
                    Self::remove_owned_path(&backup)?;
                    clone_or_copy_tree(&node_modules, &backup).map_err(|error| {
                        format!("backup dependency tree {}: {error}", node_modules.display())
                    })?;
                    Some(backup)
                } else {
                    None
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "inspect dependency rollback tree {}: {error}",
                    node_modules.display()
                ))
            }
        };
        let build_root = layout.root().join(layout.build_rel(false));
        let build_backup_result = (|| -> Result<Option<PathBuf>, String> {
            match std::fs::symlink_metadata(&build_root) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err("promoted build root must not be a symlink".into());
                    }
                    if !metadata.is_dir() {
                        return Ok(None);
                    }
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default();
                    let backup = workspace
                        .join(".lingxi-build-state")
                        .join(format!("dependency-update-rollback-build-{stamp}"));
                    Self::remove_owned_path(&backup)?;
                    clone_or_copy_tree(&build_root, &backup).map_err(|error| {
                        format!("backup promoted build {}: {error}", build_root.display())
                    })?;
                    Ok(Some(backup))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(format!(
                    "inspect dependency rollback build {}: {error}",
                    build_root.display()
                )),
            }
        })();
        let build_backup = match build_backup_result {
            Ok(backup) => backup,
            Err(error) => {
                if let Some(backup) = &node_modules_backup {
                    let _ = Self::remove_owned_path(backup);
                }
                return Err(error);
            }
        };
        Ok(DependencyUpdateRollback {
            previous_dependency,
            files,
            manifest_bytes,
            node_modules_backup,
            build_backup,
        })
    }
    pub(super) fn dependency_update_recovery_path(layout: &AppLayout) -> PathBuf {
        layout
            .root()
            .join(layout.workspace_rel())
            .join(DEPENDENCY_UPDATE_RECOVERY_FILE_REL)
    }
    pub(super) fn dependency_update_workspace(layout: &AppLayout) -> Result<PathBuf, String> {
        let workspace = layout.root().join(layout.workspace_rel());
        let metadata = std::fs::symlink_metadata(&workspace).map_err(|error| {
            format!(
                "inspect dependency update workspace {}: {error}",
                workspace.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "dependency update workspace is not a real directory: {}",
                workspace.display()
            ));
        }
        Ok(workspace)
    }
    pub(super) fn dependency_update_file_is_allowed(relative: &str) -> bool {
        matches!(
            relative,
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL
                | crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL
                | crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL
                | crate::mobile::local_app_runtime_profiles::TREE_PROOF_FILE_REL
                | crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL
                | crate::mobile::local_app_runtime_profiles::SNAPSHOT_FILE_REL
                | "package.json"
                | "pnpm-lock.yaml"
                | WORKSPACE_DEPENDENCY_ATTESTATION_FILE
        )
    }
    pub(super) fn dependency_update_backup_name(
        layout: &AppLayout,
        backup: &Path,
        kind: &str,
    ) -> Result<String, String> {
        let state_root = layout
            .root()
            .join(layout.workspace_rel())
            .join(".lingxi-build-state");
        let relative = backup.strip_prefix(&state_root).map_err(|_| {
            format!(
                "dependency rollback backup is outside the build state: {}",
                backup.display()
            )
        })?;
        let mut components = relative.components();
        let Some(Component::Normal(name)) = components.next() else {
            return Err(format!(
                "dependency rollback backup is not a single safe path: {}",
                backup.display()
            ));
        };
        if components.next().is_some() {
            return Err(format!(
                "dependency rollback backup is not a single safe path: {}",
                backup.display()
            ));
        }
        let name = name.to_str().ok_or_else(|| {
            format!(
                "dependency rollback backup name is not UTF-8: {}",
                backup.display()
            )
        })?;
        if !name.starts_with(kind) || name.len() == kind.len() {
            return Err(format!(
                "dependency rollback backup has an invalid name: {}",
                backup.display()
            ));
        }
        Ok(name.to_string())
    }
    pub(super) fn dependency_update_recovery_journal(
        layout: &AppLayout,
        rollback: &DependencyUpdateRollback,
        status: DependencyUpdateRecoveryStatus,
    ) -> Result<DependencyUpdateRecoveryJournal, String> {
        let files = rollback
            .files
            .iter()
            .map(|file| DependencyUpdateRecoveryFile {
                relative: file.relative.to_string(),
                bytes: file.bytes.clone(),
            })
            .collect();
        let journal = DependencyUpdateRecoveryJournal {
            schema_version: DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION,
            app_id: layout.app_id().to_string(),
            status,
            previous_dependency: rollback.previous_dependency.clone(),
            files,
            manifest_bytes: rollback.manifest_bytes.clone(),
            node_modules_backup: rollback
                .node_modules_backup
                .as_deref()
                .map(|backup| {
                    Self::dependency_update_backup_name(
                        layout,
                        backup,
                        "dependency-update-rollback-node_modules-",
                    )
                })
                .transpose()?,
            build_backup: rollback
                .build_backup
                .as_deref()
                .map(|backup| {
                    Self::dependency_update_backup_name(
                        layout,
                        backup,
                        "dependency-update-rollback-build-",
                    )
                })
                .transpose()?,
        };
        Self::validate_dependency_update_recovery_journal(layout, &journal)?;
        Ok(journal)
    }
    pub(super) fn dependency_update_backup_path(
        layout: &AppLayout,
        name: &str,
        kind: &str,
    ) -> Result<PathBuf, String> {
        if name.is_empty() || !name.starts_with(kind) || name.len() == kind.len() {
            return Err(format!("invalid dependency rollback backup name: {name:?}"));
        }
        let path = Path::new(name);
        let mut components = path.components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(format!(
                "dependency rollback backup must be a single path component: {name:?}"
            ));
        }
        let state_root = layout
            .root()
            .join(layout.workspace_rel())
            .join(".lingxi-build-state");
        Ok(state_root.join(name))
    }
    pub(super) fn validate_dependency_update_recovery_journal(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        if journal.schema_version != DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION {
            return Err(format!(
                "dependency update recovery journal schemaVersion {} is unsupported (expected {})",
                journal.schema_version, DEPENDENCY_UPDATE_RECOVERY_SCHEMA_VERSION
            ));
        }
        if journal.app_id != layout.app_id() {
            return Err(format!(
                "dependency update recovery journal belongs to app {}, expected {}",
                journal.app_id,
                layout.app_id()
            ));
        }
        if journal.previous_dependency.app_id != layout.app_id() {
            return Err(format!(
                "dependency update recovery record belongs to app {}, expected {}",
                journal.previous_dependency.app_id,
                layout.app_id()
            ));
        }
        if journal.previous_dependency.schema_version != local_apps::APPS_SCHEMA_VERSION {
            return Err(format!(
                "dependency update recovery record schemaVersion {} is unsupported (expected {})",
                journal.previous_dependency.schema_version,
                local_apps::APPS_SCHEMA_VERSION
            ));
        }
        if journal.files.len() > 16 {
            return Err("dependency update recovery journal has too many files".into());
        }
        let mut total_bytes = journal.manifest_bytes.len();
        let mut seen = std::collections::HashSet::new();
        for file in &journal.files {
            if !Self::dependency_update_file_is_allowed(&file.relative) {
                return Err(format!(
                    "dependency update recovery journal contains an unexpected file: {}",
                    file.relative
                ));
            }
            let path = Path::new(&file.relative);
            if file.relative.is_empty()
                || path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
                || !seen.insert(file.relative.as_str())
            {
                return Err(format!(
                    "dependency update recovery journal contains an unsafe or duplicate file: {}",
                    file.relative
                ));
            }
            total_bytes = total_bytes.saturating_add(file.bytes.as_ref().map_or(0, Vec::len));
            if total_bytes > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES {
                return Err("dependency update recovery journal is too large".into());
            }
        }
        for expected in [
            crate::mobile::local_app_runtime_profiles::REQUESTED_FILE_REL,
            crate::mobile::local_app_runtime_profiles::EFFECTIVE_PACKAGE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::LOCKFILE_FILE_REL,
            crate::mobile::local_app_runtime_profiles::TREE_PROOF_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SBOM_FILE_REL,
            crate::mobile::local_app_runtime_profiles::SNAPSHOT_FILE_REL,
            "package.json",
            "pnpm-lock.yaml",
            WORKSPACE_DEPENDENCY_ATTESTATION_FILE,
        ] {
            if !seen.iter().any(|relative| *relative == expected) {
                return Err(format!(
                    "dependency update recovery journal is missing file: {expected}"
                ));
            }
        }
        let manifest: local_apps::AppManifest = serde_json::from_slice(&journal.manifest_bytes)
            .map_err(|error| format!("parse dependency update recovery manifest: {error}"))?;
        if manifest.app_id != layout.app_id() {
            return Err(format!(
                "dependency update recovery manifest belongs to app {}, expected {}",
                manifest.app_id,
                layout.app_id()
            ));
        }
        manifest
            .validate()
            .map_err(|error| format!("validate dependency update recovery manifest: {error}"))?;
        if let Some(name) = &journal.node_modules_backup {
            Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-node_modules-",
            )?;
        }
        if let Some(name) = &journal.build_backup {
            Self::dependency_update_backup_path(layout, name, "dependency-update-rollback-build-")?;
        }
        Ok(())
    }
    pub(super) fn write_dependency_update_recovery_journal(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        Self::validate_dependency_update_recovery_journal(layout, journal)?;
        let mut bytes = serde_json::to_vec(journal)
            .map_err(|error| format!("serialize dependency update recovery journal: {error}"))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES {
            return Err("dependency update recovery journal is too large".into());
        }
        let workspace = Self::dependency_update_workspace(layout)?;
        crate::mobile::local_apps_build::write_file(
            &workspace,
            DEPENDENCY_UPDATE_RECOVERY_FILE_REL,
            &bytes,
            true,
        )
        .map_err(|error| format!("write dependency update recovery journal: {error}"))
    }
    pub(super) fn load_dependency_update_recovery_journal(
        layout: &AppLayout,
    ) -> Result<Option<DependencyUpdateRecoveryJournal>, String> {
        let _workspace = Self::dependency_update_workspace(layout)?;
        let path = Self::dependency_update_recovery_path(layout);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "inspect dependency update recovery journal {}: {error}",
                    path.display()
                ))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "dependency update recovery journal is not a regular file: {}",
                path.display()
            ));
        }
        if metadata.len() > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES as u64 {
            return Err(format!(
                "dependency update recovery journal is too large: {}",
                path.display()
            ));
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "read dependency update recovery journal {}: {error}",
                path.display()
            )
        })?;
        if bytes.len() > MAX_DEPENDENCY_UPDATE_RECOVERY_BYTES {
            return Err(format!(
                "dependency update recovery journal is too large: {}",
                path.display()
            ));
        }
        let journal: DependencyUpdateRecoveryJournal = serde_json::from_slice(&bytes)
            .map_err(|error| format!("parse dependency update recovery journal: {error}"))?;
        Self::validate_dependency_update_recovery_journal(layout, &journal)?;
        Ok(Some(journal))
    }
    pub(super) fn remove_dependency_update_recovery_journal(
        layout: &AppLayout,
    ) -> Result<(), String> {
        Self::remove_owned_path(&Self::dependency_update_recovery_path(layout))
    }
    pub(super) fn restore_dependency_update_recovery_files(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        Self::validate_dependency_update_recovery_journal(layout, journal)?;
        let workspace = Self::dependency_update_workspace(layout)?;
        for file in &journal.files {
            let path = workspace.join(&file.relative);
            match &file.bytes {
                Some(bytes) => crate::mobile::local_apps_build::write_file(
                    &workspace,
                    &file.relative,
                    bytes,
                    true,
                )
                .map_err(|error| error.to_string())?,
                None => {
                    if std::fs::symlink_metadata(&path).is_ok() {
                        Self::remove_owned_path(&path)?;
                    }
                }
            }
        }
        let manifest: local_apps::AppManifest = serde_json::from_slice(&journal.manifest_bytes)
            .map_err(|error| format!("parse dependency rollback manifest: {error}"))?;
        local_apps::save_manifest(layout, &manifest).map_err(|error| error.to_string())?;

        let node_modules = workspace.join("node_modules");
        if std::fs::symlink_metadata(&node_modules).is_ok() {
            Self::remove_owned_path(&node_modules)?;
        }
        if let Some(name) = &journal.node_modules_backup {
            let backup = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-node_modules-",
            )?;
            let metadata = std::fs::symlink_metadata(&backup).map_err(|error| {
                format!(
                    "inspect dependency rollback tree {}: {error}",
                    backup.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "dependency rollback tree is not a real directory: {}",
                    backup.display()
                ));
            }
            clone_or_copy_tree(&backup, &node_modules).map_err(|error| {
                format!(
                    "restore dependency rollback tree {}: {error}",
                    node_modules.display()
                )
            })?;
        }

        let build_root = layout.root().join(layout.build_rel(false));
        if std::fs::symlink_metadata(&build_root).is_ok() {
            Self::remove_owned_path(&build_root)?;
        }
        if let Some(name) = &journal.build_backup {
            let backup = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-build-",
            )?;
            let metadata = std::fs::symlink_metadata(&backup).map_err(|error| {
                format!(
                    "inspect dependency rollback build {}: {error}",
                    backup.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "dependency rollback build is not a real directory: {}",
                    backup.display()
                ));
            }
            clone_or_copy_tree(&backup, &build_root).map_err(|error| {
                format!(
                    "restore dependency rollback build {}: {error}",
                    build_root.display()
                )
            })?;
        }
        local_apps::storage::save_dependency_record(layout.root(), &journal.previous_dependency)
            .map_err(|error| format!("restore dependency record: {error}"))
    }
    pub(super) fn cleanup_dependency_update_recovery(
        layout: &AppLayout,
        journal: &DependencyUpdateRecoveryJournal,
    ) -> Result<(), String> {
        Self::validate_dependency_update_recovery_journal(layout, journal)?;
        let workspace = Self::dependency_update_workspace(layout)?;
        let staging = workspace.join(".lingxi-build-state/dependency-staging");
        Self::remove_owned_path(&staging)?;
        if let Some(name) = &journal.node_modules_backup {
            let path = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-node_modules-",
            )?;
            Self::remove_owned_path(&path)?;
        }
        if let Some(name) = &journal.build_backup {
            let path = Self::dependency_update_backup_path(
                layout,
                name,
                "dependency-update-rollback-build-",
            )?;
            Self::remove_owned_path(&path)?;
        }
        Self::remove_dependency_update_recovery_journal(layout)
    }
    pub(crate) fn recover_dependency_updates_on_boot(root: &Path) -> Result<(), String> {
        let apps_root = root.join("apps");
        let metadata = match std::fs::symlink_metadata(&apps_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("inspect local apps directory: {error}")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "local apps directory is not a real directory: {}",
                apps_root.display()
            ));
        }
        let mut first_error = None;
        let entries = std::fs::read_dir(&apps_root)
            .map_err(|error| format!("read local apps directory: {error}"))?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(format!("read local app entry: {error}"));
                    }
                    continue;
                }
            };
            let app_path = entry.path();
            let app_metadata = match std::fs::symlink_metadata(&app_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(format!(
                            "inspect local app entry {}: {error}",
                            app_path.display()
                        ));
                    }
                    continue;
                }
            };
            if app_metadata.file_type().is_symlink() || !app_metadata.is_dir() {
                continue;
            }
            let app_name = entry.file_name();
            let Some(app_id) = app_name.to_str() else {
                continue;
            };
            let Ok(layout) = AppLayout::new(root.to_path_buf(), app_id.to_string()) else {
                continue;
            };
            let has_journal =
                match std::fs::symlink_metadata(Self::dependency_update_recovery_path(&layout)) {
                    Ok(_) => true,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(format!(
                                "inspect dependency update recovery journal for {app_id}: {error}"
                            ));
                        }
                        false
                    }
                };
            if !has_journal {
                continue;
            }
            let recovery_result = (|| -> Result<(), String> {
                let _build_lock =
                    local_apps::storage::lock_app_build(root, app_id).map_err(|error| {
                        format!("lock app {app_id} for dependency recovery: {error}")
                    })?;
                let Some(journal) = Self::load_dependency_update_recovery_journal(&layout)? else {
                    return Ok(());
                };
                match journal.status {
                    DependencyUpdateRecoveryStatus::InProgress => {
                        Self::restore_dependency_update_recovery_files(&layout, &journal)?;
                        // Once all authoritative old state is restored, make
                        // cleanup idempotent across another crash. A committed
                        // journal means "keep what is on disk"; the on-disk
                        // state is now the old state.
                        let mut cleaned = journal.clone();
                        cleaned.status = DependencyUpdateRecoveryStatus::Committed;
                        Self::write_dependency_update_recovery_journal(&layout, &cleaned)?;
                        Self::cleanup_dependency_update_recovery(&layout, &cleaned)
                    }
                    DependencyUpdateRecoveryStatus::Committed => {
                        Self::cleanup_dependency_update_recovery(&layout, &journal)
                    }
                }
            })();
            if let Err(error) = recovery_result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    pub(super) async fn restore_dependency_update_rollback(
        &self,
        service: &Arc<AppService>,
        app_id: &str,
        layout: &AppLayout,
        rollback: &DependencyUpdateRollback,
    ) -> Result<(), String> {
        if rollback.previous_dependency.app_id != app_id {
            return Err(format!(
                "dependency rollback record belongs to app {}, expected {app_id}",
                rollback.previous_dependency.app_id
            ));
        }
        let journal = Self::dependency_update_recovery_journal(
            layout,
            rollback,
            DependencyUpdateRecoveryStatus::InProgress,
        )?;
        Self::restore_dependency_update_recovery_files(layout, &journal)?;
        service
            .restore_dependency_record(rollback.previous_dependency.clone())
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    pub(super) fn discard_dependency_update_rollback(rollback: DependencyUpdateRollback) {
        if let Some(backup) = rollback.node_modules_backup {
            if let Err(error) = Self::remove_owned_path(&backup) {
                tracing::warn!(path = %backup.display(), error = %error, "failed to remove dependency rollback tree after commit");
            }
        }
        if let Some(backup) = rollback.build_backup {
            if let Err(error) = Self::remove_owned_path(&backup) {
                tracing::warn!(path = %backup.display(), error = %error, "failed to remove build rollback tree after commit");
            }
        }
    }
}
