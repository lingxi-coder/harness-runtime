//! Native/guest coordinates for no-project scheduled sessions. The manifest is
//! host-owned; native clients use it to reopen a result without inventing a Project.
use super::MobileConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    workspace: String,
    task_id: String,
}

#[derive(Debug)]
pub(super) struct ScheduledWorkspace {
    pub host: PathBuf,
    pub logical: PathBuf,
    pub memory_host: PathBuf,
    pub memory_logical: PathBuf,
    task_id: String,
    relative: String,
}

fn directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("scheduled storage must be a real directory".into());
    }
    Ok(())
}

pub(super) fn is_global_controller(config: &MobileConfig) -> bool {
    let Some(home) = config.lingxi_home.parent() else {
        return false;
    };
    let expected = home
        .join(&config.brand_runtime_profile.brand_id)
        .join("scheduled");
    std::fs::canonicalize(&config.cwd)
        .ok()
        .zip(std::fs::canonicalize(expected).ok())
        .is_some_and(|(cwd, expected)| cwd == expected)
}

impl ScheduledWorkspace {
    pub fn prepare(
        config: &MobileConfig,
        task_id: &str,
        title: &str,
        target: Option<uuid::Uuid>,
    ) -> Result<Option<Self>, String> {
        if !is_global_controller(config) {
            return Ok(None);
        }
        let canonical_root =
            std::fs::canonicalize(&config.lingxi_home).map_err(|e| e.to_string())?;
        let root = &canonical_root;
        let manifest = if let Some(target) = target {
            let path = root
                .join("automations/sessions")
                .join(format!("{target}.json"));
            if std::fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err("invalid scheduled workspace manifest".into());
            }
            let body = match std::fs::read_to_string(&path) {
                Ok(body) => body,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.to_string()),
            };
            let record: Manifest = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let parts: Vec<_> = Path::new(&record.workspace).components().collect();
            if record.version != 1
                || parts.len() != 3
                || parts[0] != Component::Normal("workspaces".as_ref())
                || parts
                    .iter()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err("invalid scheduled workspace manifest".into());
            }
            record
        } else {
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            let slug: String = title
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .take(48)
                .collect();
            let slug = slug.trim_matches('-');
            let slug = if slug.is_empty() { "task" } else { slug };
            let relative = format!("workspaces/{date}/{slug}-{}", uuid::Uuid::new_v4());
            directory(&root.join("workspaces"))?;
            directory(&root.join("workspaces").join(date))?;
            std::fs::create_dir(root.join(&relative)).map_err(|e| e.to_string())?;
            Manifest {
                version: 1,
                workspace: relative,
                task_id: task_id.into(),
            }
        };
        let mut checked = root.clone();
        for part in Path::new(&manifest.workspace).components() {
            checked.push(part);
            let metadata = std::fs::symlink_metadata(&checked).map_err(|e| e.to_string())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("scheduled workspace is unavailable or symlinked".into());
            }
        }
        let host = checked;
        for child in ["work", "outputs"] {
            directory(&host.join(child))?;
        }
        let guest_root = if config.logical_cwd.is_some() {
            config.brand_runtime_profile.config_guest_path()
        } else {
            root.clone()
        };
        // A task selecting another task's conversation cannot inherit its memory.
        let key = format!("{:x}", Sha256::digest(task_id.as_bytes()));
        directory(&root.join("automations"))?;
        directory(&root.join("automations").join(&key))?;
        let memory_host = root.join("automations").join(&key).join("memory.md");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&memory_host)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
        let metadata = std::fs::symlink_metadata(&memory_host).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("invalid task memory file".into());
        }
        Ok(Some(Self {
            host,
            logical: guest_root.join(&manifest.workspace),
            memory_host,
            memory_logical: guest_root.join("automations").join(key).join("memory.md"),
            relative: manifest.workspace,
            task_id: task_id.into(),
        }))
    }

    pub fn bind(&self, brand_home: &Path, session_id: &str) -> Result<(), String> {
        let id = uuid::Uuid::parse_str(session_id).map_err(|e| e.to_string())?;
        directory(&brand_home.join("automations/sessions"))?;
        let path = brand_home
            .join("automations/sessions")
            .join(format!("{id}.json"));
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let record = Manifest {
            version: 1,
            workspace: self.relative.clone(),
            task_id: self.task_id.clone(),
        };
        std::fs::write(
            &temporary,
            serde_json::to_vec(&record).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(temporary, path).map_err(|e| e.to_string())
    }

    pub fn instructions(&self) -> String {
        format!("\n\nScheduled task workspace:\nUse {} for intermediate files, scripts and drafts.\nUse {} for user-facing deliverables and link saved outputs in your final response.\nRead task memory {} before working and update it with concise durable notes when useful.\nAnswer inline when files are unnecessary. Do not write directly in the home directory.",
            self.logical.join("work").display(), self.logical.join("outputs").display(), self.memory_logical.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> MobileConfig {
        let controller = root.join("atlas/scheduled");
        std::fs::create_dir_all(&controller).unwrap();
        let mut config = crate::mobile::test_support::test_config(&controller);
        config.brand_runtime_profile =
            mobile_linux_api::BrandRuntimeProfile::new("atlas", "atlas_user", ".atlas");
        config.lingxi_home = root.join(".atlas");
        std::fs::create_dir_all(&config.lingxi_home).unwrap();
        config.logical_cwd = Some("/home/atlas_user/atlas/scheduled".into());
        config
    }

    #[test]
    fn scheduled_workspace_uses_the_brand_and_reuses_the_bound_session() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let first = ScheduledWorkspace::prepare(&config, "task", "../Daily brief", None)
            .unwrap()
            .unwrap();
        let second = ScheduledWorkspace::prepare(&config, "task", "Daily brief", None)
            .unwrap()
            .unwrap();
        assert!(first.host.starts_with(
            std::fs::canonicalize(&config.lingxi_home)
                .unwrap()
                .join("workspaces")
        ));
        assert!(first
            .logical
            .starts_with("/home/atlas_user/.atlas/workspaces"));
        assert_ne!(first.host, second.host);
        assert_eq!(first.memory_host, second.memory_host);
        assert!(first.host.join("work").is_dir());
        assert!(first.host.join("outputs").is_dir());
        std::fs::write(&first.memory_host, "persistent memory").unwrap();
        let id = uuid::Uuid::new_v4();
        first.bind(&config.lingxi_home, &id.to_string()).unwrap();
        let restored = ScheduledWorkspace::prepare(&config, "task", "Changed title", Some(id))
            .unwrap()
            .unwrap();
        assert_eq!(restored.host, first.host);
        assert_eq!(restored.logical, first.logical);
        assert_eq!(
            std::fs::read_to_string(restored.memory_host).unwrap(),
            "persistent memory"
        );
    }

    #[test]
    fn scheduled_workspace_keeps_project_and_missing_target_semantics() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config(root.path());
        assert!(
            ScheduledWorkspace::prepare(&config, "task", "title", Some(uuid::Uuid::new_v4()))
                .unwrap()
                .is_none()
        );
        let project = root.path().join("atlas/projects/project/workspace");
        std::fs::create_dir_all(&project).unwrap();
        config.cwd = project;
        assert!(ScheduledWorkspace::prepare(&config, "task", "title", None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn scheduled_workspace_rejects_a_manifest_escape() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let id = uuid::Uuid::new_v4();
        let manifests = config.lingxi_home.join("automations/sessions");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join(format!("{id}.json")),
            r#"{"version":1,"workspace":"workspaces/../outside","task_id":"task"}"#,
        )
        .unwrap();
        assert!(ScheduledWorkspace::prepare(&config, "task", "title", Some(id)).is_err());
    }
}
