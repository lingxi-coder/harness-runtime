use std::path::{Component, Path, PathBuf};

use super::{MobileEngineError, SessionModeDto};
use branding::{BrandRuntimeProfile, LINGXI_RUNTIME_PROFILE};

/// Native backing paths of the Linux account. Native clients store logical
/// guest paths as identities and use these host paths only for filesystem I/O.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MobileUserDirectories {
    pub brand_id: String,
    pub linux_user: String,
    pub config_dir_name: String,
    pub home: String,
    pub guest_home: String,
    pub config_home: String,
    pub config_guest_path: String,
    pub projects: String,
    pub projects_guest_path: String,
    pub chat_cwd: String,
    pub chat_guest_cwd: String,
    pub code_cwd: String,
    pub code_guest_cwd: String,
    pub scheduled_cwd: String,
    pub scheduled_guest_cwd: String,
    pub apps: String,
    pub apps_guest_path: String,
}

/// Resolve product storage inside the runtime-created user's persistent home.
/// This lookup is pure: obtaining paths never boots Linux or creates files.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn mobile_user_directories(
    managed_root: String,
) -> Result<MobileUserDirectories, MobileEngineError> {
    mobile_user_directories_for_profile(managed_root, LINGXI_RUNTIME_PROFILE)
}

fn mobile_user_directories_for_profile(
    managed_root: String,
    profile: BrandRuntimeProfile,
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
    if !profile.has_safe_path_components() {
        return Err(MobileEngineError::Internal(
            "brand runtime profile contains an unsafe path component".into(),
        ));
    }
    let home = profile.persistent_home_host_path(root);
    let guest_home = profile.guest_home();
    let config = home.join(profile.config_dir_name);
    let config_guest_path = profile.config_guest_path();
    let workspace = branding::workspace_home(&config);
    let chat_cwd = workspace.join("chat");
    let code_cwd = workspace.join("code");
    let projects = workspace.join("projects");
    let projects_guest_path = profile.projects_guest_path();
    let scheduled = workspace.join("scheduled");
    let scheduled_guest_path = profile.workspace_container_guest_path().join("scheduled");
    let apps = workspace.join("apps");
    let apps_guest_path = profile.apps_guest_path();
    Ok(MobileUserDirectories {
        brand_id: profile.brand_id.into(),
        linux_user: profile.linux_user.into(),
        config_dir_name: profile.config_dir_name.into(),
        home: home.to_string_lossy().into_owned(),
        guest_home: guest_home.to_string_lossy().into_owned(),
        config_home: config.to_string_lossy().into_owned(),
        config_guest_path: config_guest_path.to_string_lossy().into_owned(),
        projects: projects.to_string_lossy().into_owned(),
        projects_guest_path: projects_guest_path.to_string_lossy().into_owned(),
        chat_cwd: chat_cwd.to_string_lossy().into_owned(),
        chat_guest_cwd: profile
            .chat_workspace_guest_path()
            .to_string_lossy()
            .into_owned(),
        code_cwd: code_cwd.to_string_lossy().into_owned(),
        code_guest_cwd: profile
            .code_workspace_guest_path()
            .to_string_lossy()
            .into_owned(),
        scheduled_cwd: scheduled.to_string_lossy().into_owned(),
        scheduled_guest_cwd: scheduled_guest_path.to_string_lossy().into_owned(),
        apps: apps.to_string_lossy().into_owned(),
        apps_guest_path: apps_guest_path.to_string_lossy().into_owned(),
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
            Path::new("persistent").join(&self.linux_user).as_path(),
        )?;
        let canonical_home = home.canonicalize().map_err(directory_error)?;
        for path in [
            &self.config_home,
            &self.projects,
            &self.chat_cwd,
            &self.code_cwd,
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

    /// Resolve a conversation cwd, choosing its fixed mode's Global default only
    /// when no explicit project, resumed, or scheduled cwd was supplied.
    pub fn resolve_cwd(
        &self,
        requested: Option<&str>,
        mode: SessionModeDto,
    ) -> Result<(PathBuf, PathBuf), MobileEngineError> {
        let default = match mode {
            SessionModeDto::Chat => &self.chat_cwd,
            SessionModeDto::Code => &self.code_cwd,
        };
        self.resolve_explicit_cwd(requested.unwrap_or(default))
    }

    /// Resolve an explicitly chosen host or guest cwd without inventing a
    /// conversation mode, for example for a scheduled controller. A canonical
    /// containment check rejects symlink escapes from the account.
    pub fn resolve_explicit_cwd(
        &self,
        requested: &str,
    ) -> Result<(PathBuf, PathBuf), MobileEngineError> {
        if requested.is_empty() {
            return Err(MobileEngineError::Internal(
                "workspace path must not be empty".into(),
            ));
        }
        let requested = Path::new(requested);
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
        let profile = LINGXI_RUNTIME_PROFILE;
        assert_eq!(
            Path::new(&dirs.chat_cwd),
            branding::workspace_home(Path::new(&dirs.config_home)).join("chat")
        );
        assert_eq!(
            Path::new(&dirs.code_cwd),
            branding::workspace_home(Path::new(&dirs.config_home)).join("code")
        );
        assert_eq!(dirs.brand_id, profile.brand_id);
        assert_eq!(dirs.linux_user, profile.linux_user);
        assert_eq!(dirs.config_dir_name, profile.config_dir_name);
        assert_eq!(dirs.guest_home, profile.guest_home().display().to_string());
        assert_eq!(
            dirs.config_guest_path,
            profile.config_guest_path().display().to_string()
        );
        assert_eq!(
            dirs.chat_guest_cwd,
            profile.chat_workspace_guest_path().display().to_string()
        );
        assert_eq!(
            dirs.code_guest_cwd,
            profile.code_workspace_guest_path().display().to_string()
        );
        assert_eq!(
            dirs.projects_guest_path,
            profile.projects_guest_path().display().to_string()
        );
        assert_eq!(
            dirs.apps_guest_path,
            profile.apps_guest_path().display().to_string()
        );
        assert!(!Path::new(&dirs.home).exists(), "path lookup is read-only");
        dirs.prepare().unwrap();
        assert_eq!(
            dirs.resolve_cwd(None, SessionModeDto::Chat).unwrap(),
            (
                std::fs::canonicalize(&dirs.chat_cwd).unwrap(),
                PathBuf::from(&dirs.chat_guest_cwd)
            )
        );
        assert_eq!(
            dirs.resolve_cwd(None, SessionModeDto::Code).unwrap(),
            (
                std::fs::canonicalize(&dirs.code_cwd).unwrap(),
                PathBuf::from(&dirs.code_guest_cwd)
            )
        );
        assert_eq!(
            dirs.resolve_explicit_cwd(&dirs.config_home).unwrap().1,
            PathBuf::from(&dirs.config_guest_path)
        );
        assert_eq!(
            dirs.resolve_explicit_cwd(&dirs.scheduled_cwd).unwrap().1,
            PathBuf::from(&dirs.scheduled_guest_cwd)
        );
        let source = Path::new(&dirs.projects).join("a/workspace");
        std::fs::create_dir_all(&source).unwrap();
        let (host, guest) = dirs
            .resolve_cwd(source.to_str(), SessionModeDto::Chat)
            .unwrap();
        assert_eq!(
            guest,
            Path::new(&dirs.projects_guest_path).join("a/workspace")
        );
        assert_eq!(
            dirs.resolve_cwd(guest.to_str(), SessionModeDto::Code)
                .unwrap()
                .0,
            host
        );
        assert!(Path::new(&dirs.config_home).starts_with(&dirs.home));
        assert!(dirs
            .resolve_cwd(root.path().to_str(), SessionModeDto::Chat)
            .is_err());
        assert!(dirs.resolve_cwd(Some(""), SessionModeDto::Code).is_err());
    }

    #[test]
    fn another_brand_gets_distinct_guest_and_native_paths() {
        let root = tempfile::tempdir().unwrap();
        let managed_root = root.path().join("runtime");
        let dirs = mobile_user_directories_for_profile(
            managed_root.display().to_string(),
            BrandRuntimeProfile::new("atlas", "atlas_guest", ".atlas"),
        )
        .unwrap();

        assert_eq!(
            dirs.home,
            managed_root
                .join("persistent/atlas_guest")
                .display()
                .to_string()
        );
        assert_eq!(dirs.guest_home, "/home/atlas_guest");
        assert_eq!(
            dirs.config_home,
            managed_root
                .join("persistent/atlas_guest/.atlas")
                .display()
                .to_string()
        );
        assert_eq!(dirs.config_guest_path, "/home/atlas_guest/.atlas");
        assert_eq!(
            dirs.chat_guest_cwd,
            "/home/atlas_guest/.atlas-workspaces/chat"
        );
        assert_eq!(
            dirs.code_guest_cwd,
            "/home/atlas_guest/.atlas-workspaces/code"
        );
        assert_eq!(
            dirs.projects_guest_path,
            "/home/atlas_guest/.atlas-workspaces/projects"
        );
        assert_eq!(
            dirs.apps_guest_path,
            "/home/atlas_guest/.atlas-workspaces/apps"
        );

        dirs.prepare().unwrap();
        assert!(Path::new(&dirs.projects).is_dir());
        assert!(Path::new(&dirs.apps).is_dir());
        assert!(Path::new(&dirs.home).starts_with(managed_root.join("persistent/atlas_guest")));
    }

    #[test]
    fn profile_path_components_are_checked_before_path_construction() {
        let root = tempfile::tempdir().unwrap();
        assert!(mobile_user_directories_for_profile(
            root.path().join("runtime").display().to_string(),
            BrandRuntimeProfile::new(
                "../outside",
                LINGXI_RUNTIME_PROFILE.linux_user,
                LINGXI_RUNTIME_PROFILE.config_dir_name,
            ),
        )
        .is_err());
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
        assert!(dirs
            .resolve_cwd(link.to_str(), SessionModeDto::Chat)
            .is_err());
    }
}
