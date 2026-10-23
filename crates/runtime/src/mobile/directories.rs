use std::path::{Component, Path, PathBuf};

use super::MobileEngineError;
use mobile_linux_api::guest_paths;

/// Native backing paths of the Linux account. Native clients store logical
/// guest paths as identities and use these host paths only for filesystem I/O.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MobileUserDirectories {
    pub home: String,
    pub guest_home: String,
    pub config_home: String,
    pub projects: String,
    pub projects_guest_path: String,
    pub default_cwd: String,
    pub default_guest_cwd: String,
    pub scheduled_cwd: String,
    pub scheduled_guest_cwd: String,
    pub apps: String,
}

/// Resolve product storage inside the runtime-created user's persistent home.
/// This lookup is pure: obtaining paths never boots Linux or creates files.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn mobile_user_directories(
    managed_root: String,
) -> Result<MobileUserDirectories, MobileEngineError> {
    let root = Path::new(&managed_root);
    if !root.is_absolute()
        || managed_root.contains('\0')
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(MobileEngineError::Internal(
            "runtime storage root must be an absolute normalized path".into(),
        ));
    }
    let home = guest_paths::home_host_path(root);
    let workspace_relative = Path::new(guest_paths::WORKSPACE_ROOT)
        .strip_prefix(guest_paths::HOME)
        .expect("runtime workspace root is in the runtime user home");
    let cwd = home.join(workspace_relative);
    let config = home.join(branding::DOT_DIR);
    Ok(MobileUserDirectories {
        home: home.to_string_lossy().into_owned(),
        guest_home: guest_paths::HOME.into(),
        config_home: config.to_string_lossy().into_owned(),
        projects: cwd.join("projects").to_string_lossy().into_owned(),
        projects_guest_path: guest_paths::workspace("projects"),
        default_cwd: cwd.to_string_lossy().into_owned(),
        default_guest_cwd: guest_paths::WORKSPACE_ROOT.into(),
        scheduled_cwd: cwd.join("scheduled").to_string_lossy().into_owned(),
        scheduled_guest_cwd: guest_paths::workspace("scheduled"),
        apps: config.join(".apps").to_string_lossy().into_owned(),
    })
}

impl MobileUserDirectories {
    /// Conversations share a stable mount of this account's projects directory.
    /// A selected cwd must never replace it with an unrelated host directory.
    pub fn validate_projects_mount(
        &self,
        host_path: &str,
        stable_id: &str,
    ) -> Result<(), MobileEngineError> {
        if stable_id != "projects"
            || std::fs::canonicalize(host_path).map_err(directory_error)?
                != std::fs::canonicalize(&self.projects).map_err(directory_error)?
        {
            return Err(MobileEngineError::Internal(
                "conversation workspace mount must be the Linux user's projects directory".into(),
            ));
        }
        Ok(())
    }

    /// Prepare only user-owned directories. Rootfs repair may replace its
    /// images without removing projects, transcripts, or application sources.
    pub fn prepare(&self) -> Result<(), MobileEngineError> {
        let home = Path::new(&self.home);
        let managed = home
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| MobileEngineError::Internal("invalid Linux user home".into()))?;
        std::fs::create_dir_all(managed).map_err(directory_error)?;
        let managed = managed.canonicalize().map_err(directory_error)?;
        create_owned_directory(
            &managed,
            Path::new("persistent").join(guest_paths::USER).as_path(),
        )?;
        let canonical_home = home.canonicalize().map_err(directory_error)?;
        for path in [
            &self.config_home,
            &self.projects,
            &self.default_cwd,
            &self.scheduled_cwd,
            &self.apps,
        ] {
            let relative = Path::new(path).strip_prefix(home).map_err(|_| {
                MobileEngineError::Internal("user directory escaped Linux home".into())
            })?;
            create_owned_directory(&canonical_home, relative)?;
        }
        Ok(())
    }

    /// Validate a host or logical guest cwd and return both coordinates. A
    /// canonical containment check rejects symlink escapes from the account.
    pub fn resolve_cwd(
        &self,
        requested: Option<&str>,
    ) -> Result<(PathBuf, PathBuf), MobileEngineError> {
        let requested = Path::new(
            requested
                .filter(|path| !path.is_empty())
                .unwrap_or(&self.default_cwd),
        );
        let requested = if let Ok(relative) = requested.strip_prefix(&self.guest_home) {
            Path::new(&self.home).join(relative)
        } else {
            requested.to_path_buf()
        };
        let home = std::fs::canonicalize(&self.home).map_err(directory_error)?;
        let host = std::fs::canonicalize(requested).map_err(directory_error)?;
        let relative = host.strip_prefix(&home).map_err(|_| {
            MobileEngineError::Internal("workspace must remain inside the Linux user's home".into())
        })?;
        if !host.is_dir() {
            return Err(MobileEngineError::Internal(
                "workspace is not a directory".into(),
            ));
        }
        Ok((host.clone(), Path::new(&self.guest_home).join(relative)))
    }
}

fn create_owned_directory(root: &Path, relative: &Path) -> Result<(), MobileEngineError> {
    let mut directory = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(MobileEngineError::Internal(
                "user storage path must be normalized".into(),
            ));
        };
        directory.push(name);
        match std::fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(directory_error(error)),
        }
        let metadata = std::fs::symlink_metadata(&directory).map_err(directory_error)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(MobileEngineError::Internal(
                "Linux user storage may not traverse a symlink".into(),
            ));
        }
    }
    Ok(())
}

fn directory_error(error: std::io::Error) -> MobileEngineError {
    MobileEngineError::Internal(format!("Linux user directory is unavailable: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_and_identity_share_the_runtime_account() {
        let root = tempfile::tempdir().unwrap();
        let dirs =
            mobile_user_directories(root.path().join("runtime").display().to_string()).unwrap();
        assert!(!Path::new(&dirs.home).exists(), "path lookup is read-only");
        dirs.prepare().unwrap();
        let source = Path::new(&dirs.projects).join("a/workspace");
        std::fs::create_dir_all(&source).unwrap();
        let (host, guest) = dirs.resolve_cwd(source.to_str()).unwrap();
        assert_eq!(
            guest,
            Path::new(&dirs.projects_guest_path).join("a/workspace")
        );
        assert_eq!(dirs.resolve_cwd(guest.to_str()).unwrap().0, host);
        assert!(Path::new(&dirs.config_home).starts_with(&dirs.home));
        assert!(dirs.resolve_cwd(root.path().to_str()).is_err());
    }
    #[test]
    fn conversation_mount_cannot_replace_the_account_projects_directory() {
        let root = tempfile::tempdir().unwrap();
        let dirs =
            mobile_user_directories(root.path().join("runtime").display().to_string()).unwrap();
        dirs.prepare().unwrap();
        dirs.validate_projects_mount(&dirs.projects, "projects")
            .unwrap();
        assert!(dirs
            .validate_projects_mount(&dirs.home, "projects")
            .is_err());
        assert!(dirs
            .validate_projects_mount(&dirs.projects, "other")
            .is_err());
        assert!(dirs
            .validate_projects_mount(root.path().to_str().unwrap(), "projects")
            .is_err());
        assert!(dirs.validate_projects_mount("", "projects").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn workspace_cannot_escape_the_user_through_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let dirs =
            mobile_user_directories(root.path().join("runtime").display().to_string()).unwrap();
        dirs.prepare().unwrap();
        let link = Path::new(&dirs.projects).join("escape");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        assert!(dirs.resolve_cwd(link.to_str()).is_err());
    }
}
