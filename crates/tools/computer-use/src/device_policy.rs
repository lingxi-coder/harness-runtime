//! Desktop's host-owned, atomically replaced device policy. Never a tool argument.
use super::{app_policy, permission_model::AppTier, ToolError};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::Path};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceApp {
    pub bundle_id: String,
    pub display_name: String,
    pub permission: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevicePolicy {
    pub revision: String,
    pub enabled: bool,
    pub apps: Vec<DeviceApp>,
    #[serde(default)]
    pub source: Option<PolicySource>,
    #[serde(default)]
    pub owner_pid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PolicySource {
    pub path: std::path::PathBuf,
    pub sha256: String,
}

impl DevicePolicy {
    pub fn denies(&self, id: &str, name: &str) -> bool {
        id == "com.lingxi.code"
            || id.starts_with("com.lingxi.code.")
            || app_policy::blocked(Some(id), name)
            || self
                .apps
                .iter()
                .any(|app| app.bundle_id == id && app.permission == "deny")
    }
    pub fn permanent_grants(&self) -> impl Iterator<Item = (&str, AppTier)> {
        self.apps
            .iter()
            .filter(|app| {
                app.permission == "allow" && !self.denies(&app.bundle_id, &app.display_name)
            })
            .map(|app| {
                (
                    app.bundle_id.as_str(),
                    app_policy::maximum_tier(Some(&app.bundle_id), &app.display_name),
                )
            })
    }
}

fn private_bytes(path: &Path) -> Result<Vec<u8>, ToolError> {
    let denied = || {
        ToolError::PermissionDenied("Computer access settings are unavailable. Restore them in Desktop settings before retrying.".into())
    };
    let before = std::fs::symlink_metadata(path).map_err(|_| denied())?;
    if !before.file_type().is_file() || before.len() > 262_144 {
        return Err(denied());
    }
    let file = File::open(path).map_err(|_| denied())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let after = file.metadata().map_err(|_| denied())?;
        if before.ino() != after.ino()
            || before.dev() != after.dev()
            || after.mode() & 0o077 != 0
            || after.uid() != nix::unistd::geteuid().as_raw()
        {
            return Err(denied());
        }
    }
    let mut bytes = Vec::new();
    file.take(262_145)
        .read_to_end(&mut bytes)
        .map_err(|_| denied())?;
    if bytes.len() > 262_144 {
        return Err(denied());
    }
    Ok(bytes)
}

/// The digest, rather than a caller-supplied revision, invalidates all old grants.
/// Host envelopes additionally bind the exact user-configuration source bytes.
pub fn read(path: &Path) -> Result<(String, DevicePolicy), ToolError> {
    let denied = || {
        ToolError::PermissionDenied("Computer access settings are unavailable. Restore them in Desktop settings before retrying.".into())
    };
    let bytes = private_bytes(path)?;
    let policy: DevicePolicy = serde_json::from_slice(&bytes).map_err(|_| denied())?;
    #[cfg(unix)]
    if let Some(pid) = policy.owner_pid {
        let pid = i32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(denied)?;
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            return Err(denied());
        }
    }
    if let Some(source) = &policy.source {
        if !source.path.is_absolute()
            || source.path == path
            || source.sha256.len() != 64
            || !source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(denied());
        }
        let current = private_bytes(&source.path)?;
        if format!("{:x}", Sha256::digest(&current)) != source.sha256 {
            return Err(denied());
        }
    }
    if policy.revision.is_empty()
        || policy.apps.len() > 512
        || policy.apps.iter().any(|app| {
            app.bundle_id.is_empty()
                || app.bundle_id.len() > 255
                || app.display_name.len() > 512
                || !matches!(app.permission.as_str(), "ask" | "allow" | "deny")
        })
    {
        return Err(denied());
    }
    Ok((format!("{:x}", Sha256::digest(&bytes)), policy))
}
