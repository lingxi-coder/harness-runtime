//! Durable device library. A publication never mutates the author's working tree.
use super::*;
use fs2::FileExt;
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RELEASE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Persistent local app library.
#[derive(Debug, Clone)]
pub struct AppLibrary {
    config_home: PathBuf,
    root: PathBuf,
}

/// Exclusive process-lifecycle ownership for one device library.
///
/// Keep this guard alive for the host's entire lifetime. Dropping it releases
/// the OS lock; it does not terminate processes. A bridge owner must therefore
/// finish native child cleanup before reconnecting and acquiring another lease.
#[derive(Debug)]
#[must_use = "the runtime owner must retain this lease until all owned processes stop"]
pub struct RuntimeLease {
    _file: File,
    /// A diagnostic generation, never a PID or a credential for an app page.
    pub owner_id: String,
    /// Historical live records changed to Interrupted after exclusive ownership.
    pub recovered_runs: usize,
    /// Unix milliseconds when exclusive ownership was obtained.
    pub acquired_at_ms: u64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn id() -> String {
    format!("a{}", Uuid::new_v4().simple())
}
fn valid_id(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Err(AppError::Invalid("invalid app or run id".into()))
    } else {
        Ok(())
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn ensure_directory(path: &Path) -> Result<(), AppError> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(AppError::Invalid(format!(
                "not a real directory: {}",
                path.display()
            )));
        }
    } else {
        if let Some(parent) = path.parent() {
            ensure_directory(parent)?;
        }
        fs::create_dir(path)?;
    }
    Ok(())
}
fn checked_file(root: &Path, relative: &str) -> Result<PathBuf, AppError> {
    relative_path(relative)?;
    let mut current = root.to_path_buf();
    for (i, part) in relative.split('/').enumerate() {
        current.push(part);
        let meta = fs::symlink_metadata(&current)?;
        if meta.file_type().is_symlink() {
            return Err(AppError::Invalid(format!(
                "symlink is not allowed: {relative}"
            )));
        }
        if i + 1 < relative.split('/').count() && !meta.is_dir() {
            return Err(AppError::Invalid("non-directory path component".into()));
        }
    }
    if !current.is_file() {
        return Err(AppError::Invalid(format!(
            "not a regular source file: {relative}"
        )));
    }
    Ok(current)
}
fn bytes(path: &Path) -> Result<Vec<u8>, AppError> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_FILE_BYTES {
        return Err(AppError::Invalid(format!(
            "invalid or oversized file {}",
            path.display()
        )));
    }
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err(AppError::Invalid("file grew past size limit".into()));
    }
    Ok(data)
}
fn executable(path: &Path) -> Result<bool, AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o100 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}
fn apply_executable(path: &Path, enabled: bool) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(if enabled { 0o744 } else { 0o644 });
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    let _ = (path, enabled);
    Ok(())
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, AppError> {
    Ok(serde_json::from_slice(&bytes(path)?)?)
}
fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    atomic_bytes(path, &serde_json::to_vec_pretty(value)?)
}
fn atomic_bytes(path: &Path, data: &[u8]) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Invalid("missing parent".into()))?;
    ensure_directory(parent)?;
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(AppError::Invalid("refusing symlink write".into()));
    }
    let tmp = parent.join(format!(".write-{}", Uuid::new_v4().simple()));
    let result = (|| {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok::<_, AppError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
fn excluded(path: &str) -> bool {
    path.split('/').any(|part| {
        let p = part.to_ascii_lowercase();
        matches!(
            p.as_str(),
            ".git"
                | ".hg"
                | ".svn"
                | "node_modules"
                | ".venv"
                | "venv"
                | "__pycache__"
                | ".ssh"
                | ".aws"
                | ".codex"
                | ".lingxi"
                | ".npmrc"
                | ".pypirc"
                | "credentials"
                | "credentials.json"
                | "secrets"
                | "secrets.json"
                | "browser-data"
                | "sessions"
                | "transcripts"
                | "runtime-data"
                | ".ds_store"
        ) || p == ".env"
            || p.starts_with(".env.")
            || p.ends_with(".pem")
            || p.ends_with(".key")
            || p.ends_with(".sqlite")
            || p.ends_with(".sqlite3")
            || p.ends_with(".db")
            || p.ends_with(".db-wal")
            || p.ends_with(".db-shm")
    })
}
fn enumerate(root: &Path, dir: &Path, result: &mut Vec<String>) -> Result<(), AppError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| AppError::Invalid("source escaped root".into()))?
            .to_str()
            .ok_or_else(|| AppError::Invalid("source paths must be UTF-8".into()))?
            .replace('\\', "/");
        if excluded(&relative) {
            continue;
        }
        relative_path(&relative)?;
        let meta = entry.file_type()?;
        if meta.is_symlink() {
            return Err(AppError::Invalid(format!("source symlink: {relative}")));
        }
        if meta.is_dir() {
            enumerate(root, &entry.path(), result)?;
        } else if meta.is_file() {
            result.push(relative);
            if result.len() > MAX_FILES {
                return Err(AppError::Invalid("too many package files".into()));
            }
        } else {
            return Err(AppError::Invalid(
                "special files cannot be published".into(),
            ));
        }
    }
    Ok(())
}
fn digest(manifest: &Manifest, files: &[ReleaseFile]) -> Result<String, AppError> {
    Ok(hash(&serde_json::to_vec(&(manifest, files))?))
}
fn validate_authoring_source(authoring: &AuthoringRef) -> Result<(), AppError> {
    validate_canonical_absolute_path(&authoring.source_directory, "authoring sourceDirectory")
}

fn validate_canonical_absolute_path(path: &str, label: &str) -> Result<(), AppError> {
    if !path.starts_with('/')
        || path.contains(['\0', '\\'])
        || path.split('/').any(|part| part == ".." || part == ".")
    {
        return Err(AppError::Invalid(format!(
            "{label} must be an absolute canonical guest or host path"
        )));
    }
    Ok(())
}

fn valid_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn authoring_binding_complete(authoring: &AuthoringRef) -> bool {
    authoring
        .workspace_id
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        && authoring.project_ref.is_some()
        && authoring
            .authoring_session_id
            .as_deref()
            .is_some_and(|value| !value.is_empty())
}

fn validate_authoring_binding(authoring: &AuthoringRef) -> Result<(), AppError> {
    validate_authoring_source(authoring)?;
    let workspace_id = authoring.workspace_id.as_deref().ok_or_else(|| {
        AppError::Invalid("authoring workspaceId is required before binding".into())
    })?;
    if !valid_uuid(workspace_id) {
        return Err(AppError::Invalid(
            "authoring workspaceId must be a canonical UUID".into(),
        ));
    }
    match authoring.project_ref.as_ref().ok_or_else(|| {
        AppError::Invalid("authoring projectRef is required before binding".into())
    })? {
        ProjectRef::DesktopPath { path } => {
            // The desktop project identity can be an existing project root
            // that contains the app source directory; both are independently
            // supplied and verified by the desktop host adapter.
            if !path.starts_with('/')
                || path.contains(['\0', '\\'])
                || path.split('/').any(|part| part == "." || part == "..")
            {
                return Err(AppError::Invalid(
                    "desktop projectRef.path must be an absolute canonical path".into(),
                ));
            }
        }
        ProjectRef::MobileProject { project_id } => {
            if !valid_uuid(project_id) {
                return Err(AppError::Invalid(
                    "mobile projectRef.projectId must be a canonical UUID".into(),
                ));
            }
        }
    }
    let session_id = authoring.authoring_session_id.as_deref().ok_or_else(|| {
        AppError::Invalid("authoring authoringSessionId is required before binding".into())
    })?;
    if !valid_uuid(session_id) {
        return Err(AppError::Invalid(
            "authoring authoringSessionId must be a canonical engine UUID".into(),
        ));
    }
    if let Some(app_id) = authoring.app_id.as_deref() {
        valid_id(app_id)?;
    }
    Ok(())
}

fn validate_authoring_owner(authoring: &AuthoringRef, app_id: &str) -> Result<(), AppError> {
    if authoring
        .app_id
        .as_deref()
        .is_some_and(|owner| owner != app_id)
    {
        return Err(AppError::Invalid(
            "authoring appId does not match its owning application".into(),
        ));
    }
    Ok(())
}

fn authoring_with_owner(mut authoring: AuthoringRef, app_id: &str) -> AuthoringRef {
    authoring.app_id = Some(app_id.to_owned());
    authoring
}
fn upgrade_desktop_manifest(mut value: serde_json::Value) -> Result<Manifest, AppError> {
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(2) => return Manifest::parse(&value.to_string()),
        Some(1) => {}
        _ => {
            return Err(AppError::Invalid(
                "unsupported legacy desktop manifest schema".into(),
            ))
        }
    }
    value["schema"] = serde_json::json!(2);
    if value["entry"]["type"] == "server" {
        if let Some(path) = value["entry"]["path"].as_str() {
            if !path.starts_with('/') {
                value["entry"]["path"] = serde_json::json!(format!("/{path}"));
            }
        }
    }
    let executes = ["install", "build", "processes"]
        .iter()
        .any(|key| value[*key].as_array().is_some_and(|v| !v.is_empty()))
        || value["entry"]["type"] == "server"
        || value["capabilities"]
            .as_array()
            .is_some_and(|v| v.iter().any(|c| c == "process"));
    let targets = if executes {
        let platform = std::env::consts::OS;
        if !["macos", "windows", "linux"].contains(&platform) {
            return Err(AppError::Invalid(
                "executable desktop archives must first be imported on their desktop host".into(),
            ));
        }
        vec![serde_json::json!({"platform":platform,"environment":"native"})]
    } else {
        ["android", "ios", "macos", "windows", "linux", "web"]
            .into_iter()
            .map(|platform| serde_json::json!({"platform":platform,"environment":"web"}))
            .collect()
    };
    value["targets"] = serde_json::Value::Array(targets);
    Manifest::parse(&value.to_string())
}
impl AppLibrary {
    /// Open the persistent store at the supplied root.
    pub fn open(config_home: impl AsRef<Path>) -> Result<Self, AppError> {
        let supplied = config_home.as_ref();
        if !supplied.is_absolute() {
            return Err(AppError::Invalid("config home must be absolute".into()));
        }
        ensure_directory(supplied)?;
        let config_home = fs::canonicalize(supplied)?;
        let root = config_home.join("local-apps/v2");
        ensure_directory(&root)?;
        ensure_directory(&root.join("apps"))?;
        ensure_directory(&root.join("transactions"))?;
        ensure_directory(&root.join("pending-authoring"))?;
        Ok(Self { config_home, root })
    }
    /// Return the library root path.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Acquire exclusive runtime ownership without waiting for another host.
    ///
    /// Only a successful owner acquisition recovers records from a previous
    /// host. Merely opening a library, or reconnecting a reader, never changes
    /// live state. Recovery marks records interrupted; it never restores or
    /// terminates an old PID. Hosts must not recreate the bridge while its old
    /// native children are still owned and alive.
    pub fn acquire_runtime_lease(&self) -> Result<RuntimeLease, AppError> {
        let path = self.root.join("runtime-owner.lock");
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Err(AppError::Invalid("symlink runtime owner lock".into()));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(AppError::Busy(
                    "another host owns this local app runtime".into(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
        let owner_id = id();
        let acquired_at_ms = now();
        let recovered_runs = self.recover_interrupted_runs()?;
        // Do not rename a locked file: every contender must lock the same inode.
        // This metadata is diagnostic only and is never trusted to kill a PID.
        let metadata = serde_json::to_vec(&serde_json::json!({
            "ownerId": owner_id,
            "processId": std::process::id(),
            "acquiredAtMs": acquired_at_ms,
            "recoveredRuns": recovered_runs,
        }))?;
        file.set_len(0)?;
        file.write_all(&metadata)?;
        file.sync_all()?;
        Ok(RuntimeLease {
            _file: file,
            owner_id,
            recovered_runs,
            acquired_at_ms,
        })
    }
    /// Return the default authoring workspace root.
    pub fn default_authoring_root(&self) -> PathBuf {
        self.config_home.join(".apps")
    }
    fn lock(&self) -> Result<File, AppError> {
        let path = self.root.join(".lock");
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(AppError::Invalid("symlink library lock".into()));
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        Ok(file)
    }
    fn app_path(&self, app_id: &str) -> Result<PathBuf, AppError> {
        valid_id(app_id)?;
        let path = self.root.join("apps").join(app_id);
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(AppError::Invalid("symlink app directory".into()));
        }
        Ok(path)
    }
    fn record_path(&self, app_id: &str) -> Result<PathBuf, AppError> {
        Ok(self.app_path(app_id)?.join("record.json"))
    }
    fn get_any(&self, app_id: &str) -> Result<AppRecord, AppError> {
        let path = self.record_path(app_id)?;
        if !path.exists() {
            return Err(AppError::NotFound(app_id.into()));
        }
        let record: AppRecord = read_json(&path)?;
        if record.app_id != app_id {
            return Err(AppError::Invalid("record identity mismatch".into()));
        }
        Ok(record)
    }
    /// Load the requested record.
    pub fn get(&self, app_id: &str) -> Result<AppRecord, AppError> {
        let record = self.get_any(app_id)?;
        if record.removed {
            return Err(AppError::NotFound(app_id.into()));
        }
        Ok(record)
    }
    /// List non-removed apps in last-updated order.
    pub fn list(&self) -> Result<Vec<AppRecord>, AppError> {
        let mut result = vec![];
        for entry in fs::read_dir(self.root.join("apps"))? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                return Err(AppError::Invalid("symlink app directory".into()));
            }
            if entry.file_type()?.is_dir() && entry.path().join("record.json").exists() {
                let record = self.get_any(&entry.file_name().to_string_lossy())?;
                if !record.removed {
                    result.push(record);
                }
            }
        }
        result.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then(a.app_id.cmp(&b.app_id))
        });
        Ok(result)
    }
    fn save(&self, record: &AppRecord) -> Result<(), AppError> {
        atomic_json(&self.record_path(&record.app_id)?, record)
    }
    fn pending_authoring_path(&self, source_directory: &str) -> PathBuf {
        self.root
            .join("pending-authoring")
            .join(format!("{}.json", hash(source_directory.as_bytes())))
    }
    fn source_is_bound(&self, source_directory: &str) -> Result<bool, AppError> {
        Ok(self.list()?.iter().any(|record| {
            record
                .authoring
                .as_ref()
                .is_some_and(|authoring| authoring.source_directory == source_directory)
        }))
    }
    /// Persist the native Project/Code Session binding before its first publish.
    /// The pending row is keyed by the canonical source identity and is only
    /// consumed after an initial App record commits successfully.
    pub fn register_pending_authoring(
        &self,
        authoring: AuthoringRef,
    ) -> Result<AuthoringRef, AppError> {
        if authoring.app_id.is_some() {
            return Err(AppError::Invalid(
                "pending authoring cannot already belong to an application".into(),
            ));
        }
        validate_authoring_binding(&authoring)?;
        let _lock = self.lock()?;
        if self.source_is_bound(&authoring.source_directory)? {
            return Err(AppError::Invalid(
                "authoring source is already bound to a published application".into(),
            ));
        }
        atomic_json(
            &self.pending_authoring_path(&authoring.source_directory),
            &authoring,
        )?;
        Ok(authoring)
    }
    /// Return the source path for a release.
    pub fn source_path(&self, app_id: &str, revision: u64) -> Result<PathBuf, AppError> {
        if revision == 0 {
            return Err(AppError::Invalid("revision must be positive".into()));
        }
        Ok(self
            .app_path(app_id)?
            .join("releases")
            .join(revision.to_string())
            .join("source"))
    }
    /// Return the app data directory path.
    pub fn data_path(&self, app_id: &str) -> Result<PathBuf, AppError> {
        self.get(app_id)?;
        Ok(self.app_path(app_id)?.join("data"))
    }
    /// Load an app release.
    pub fn release(&self, app_id: &str, revision: u64) -> Result<AppRelease, AppError> {
        let record = self.get(app_id)?;
        if revision == 0 || revision > record.latest_revision {
            return Err(AppError::NotFound(format!("{app_id}@{revision}")));
        }
        let release: AppRelease = read_json(
            &self
                .app_path(app_id)?
                .join("releases")
                .join(revision.to_string())
                .join("release.json"),
        )?;
        if release.app_id != app_id || release.revision != revision {
            return Err(AppError::Invalid("release identity mismatch".into()));
        }
        release.manifest.validate()?;
        Ok(release)
    }
    /// List app releases.
    pub fn releases(&self, app_id: &str) -> Result<Vec<AppRelease>, AppError> {
        let record = self.get(app_id)?;
        (1..=record.latest_revision)
            .map(|revision| self.release(app_id, revision))
            .collect()
    }
    /// Return filesystem paths for an app release.
    pub fn layout(&self, app_id: &str, revision: u64) -> Result<AppLayout, AppError> {
        self.release(app_id, revision)?;
        let app = self.app_path(app_id)?;
        Ok(AppLayout {
            source_path: self.source_path(app_id, revision)?.to_string_lossy().into(),
            data_path: app.join("data").to_string_lossy().into(),
            releases_path: app.join("releases").to_string_lossy().into(),
            runs_path: app.join("runs").to_string_lossy().into(),
        })
    }
    /// Publish an app release.
    pub fn publish(&self, request: PublishRequest) -> Result<PublishResult, AppError> {
        self.publish_inner(request, None)
    }

    /// Initial mobile publication may adopt only a native-registered binding
    /// for the exact canonical guest source directory supplied by the host.
    pub fn publish_with_pending_authoring(
        &self,
        request: PublishRequest,
        source_directory: &str,
    ) -> Result<PublishResult, AppError> {
        validate_canonical_absolute_path(source_directory, "pending sourceDirectory")?;
        if request.app_id.is_some() || request.authoring.is_some() {
            return Err(AppError::Invalid(
                "pending authoring is only valid for an unbound initial publication".into(),
            ));
        }
        self.publish_inner(request, Some(source_directory))
    }

    fn publish_inner(
        &self,
        request: PublishRequest,
        pending_source_directory: Option<&str>,
    ) -> Result<PublishResult, AppError> {
        if let Some(a) = &request.authoring {
            validate_authoring_source(a)?;
            if authoring_binding_complete(a) {
                validate_authoring_binding(a)?;
            }
        }
        let _lock = self.lock()?;
        let pending_authoring = if let Some(source_directory) = pending_source_directory {
            if self.source_is_bound(source_directory)? {
                return Err(AppError::Invalid(
                    "authoring source is already bound to a published application".into(),
                ));
            }
            let path = self.pending_authoring_path(source_directory);
            if path.exists() {
                let authoring: AuthoringRef = read_json(&path)?;
                if authoring.app_id.is_some() || authoring.source_directory != source_directory {
                    return Err(AppError::Invalid(
                        "pending authoring identity does not match the published source".into(),
                    ));
                }
                validate_authoring_binding(&authoring)?;
                Some((path, authoring))
            } else {
                return Err(AppError::Invalid(
                    "no trusted authoring registration exists for this source directory".into(),
                ));
            }
        } else {
            None
        };
        let mut record = if let Some(app_id) = &request.app_id {
            let record = self.get(app_id)?;
            if record.latest_revision != request.expected_revision {
                return Err(AppError::Conflict {
                    expected: request.expected_revision,
                    actual: record.latest_revision,
                });
            }
            if request.authoring.as_ref().is_some_and(|a| {
                record
                    .authoring
                    .as_ref()
                    .is_some_and(|old| old.source_directory != a.source_directory)
            }) {
                return Err(AppError::Invalid(
                    "bindAuthoring must explicitly change an existing source directory".into(),
                ));
            }
            record
        } else {
            if request.expected_revision != 0 {
                return Err(AppError::Conflict {
                    expected: request.expected_revision,
                    actual: 0,
                });
            }
            let used: BTreeSet<_> = fs::read_dir(self.root.join("apps"))?
                .filter_map(Result::ok)
                .filter(|e| e.path().join("record.json").exists())
                .map(|e| {
                    self.get_any(&e.file_name().to_string_lossy())
                        .map(|r| r.origin_port)
                })
                .collect::<Result<_, _>>()?;
            let offset = (Uuid::new_v4().as_u128() % 40000) as u16;
            let origin_port = (0..40000u16)
                .map(|n| 20000 + ((u32::from(offset) + u32::from(n)) % 40000) as u16)
                .find(|p| !used.contains(p))
                .ok_or_else(|| AppError::Busy("no stable origin port available".into()))?;
            let timestamp = now();
            AppRecord {
                app_id: id(),
                name: String::new(),
                summary: String::new(),
                current_revision: 1,
                latest_revision: 0,
                authoring: None,
                market_origin: request.market_origin.clone(),
                origin_port,
                data_schema_version: 1,
                execution_grants: vec![],
                created_at_ms: timestamp,
                updated_at_ms: timestamp,
                removed: false,
            }
        };
        let source = PathBuf::from(&request.source_path);
        if !source.is_absolute() {
            return Err(AppError::Invalid("sourcePath must be absolute".into()));
        }
        let meta = fs::symlink_metadata(&source)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(AppError::Invalid(
                "sourcePath must be a real directory".into(),
            ));
        }
        let source = fs::canonicalize(source)?;
        if source.starts_with(&self.root) || self.root.starts_with(&source) {
            return Err(AppError::Invalid(
                "authoring source cannot contain or live in the app library".into(),
            ));
        }
        let manifest = Manifest::parse(
            std::str::from_utf8(&bytes(&checked_file(&source, MANIFEST_FILE)?)?)
                .map_err(|_| AppError::Invalid("manifest must be UTF-8".into()))?,
        )?;
        let mut paths = if let Some(package) = &manifest.package {
            package.files.clone()
        } else {
            let mut paths = vec![];
            enumerate(&source, &source, &mut paths)?;
            paths
        };
        if !paths.iter().any(|p| p == MANIFEST_FILE) {
            paths.push(MANIFEST_FILE.into());
        }
        paths.sort();
        paths.dedup();
        if paths.len() > MAX_FILES {
            return Err(AppError::Invalid("too many package files".into()));
        }
        let revision = record
            .latest_revision
            .checked_add(1)
            .ok_or_else(|| AppError::Invalid("revision overflow".into()))?;
        let stage = self.root.join("transactions").join(id());
        ensure_directory(&stage.join("source"))?;
        let staged = (|| {
            let mut files = vec![];
            let mut total = 0u64;
            for path in &paths {
                if excluded(path) {
                    return Err(AppError::Invalid(format!(
                        "private/generated file cannot be packaged: {path}"
                    )));
                }
                let content = if path == MANIFEST_FILE {
                    serde_json::to_vec_pretty(&manifest)?
                } else {
                    bytes(&checked_file(&source, path)?)?
                };
                total = total
                    .checked_add(content.len() as u64)
                    .ok_or_else(|| AppError::Invalid("release size overflow".into()))?;
                if total > MAX_RELEASE_BYTES {
                    return Err(AppError::Invalid("release exceeds 256 MiB".into()));
                }
                atomic_bytes(&stage.join("source").join(path), &content)?;
                let executable =
                    path != MANIFEST_FILE && executable(&checked_file(&source, path)?)?;
                apply_executable(&stage.join("source").join(path), executable)?;
                files.push(ReleaseFile {
                    path: path.clone(),
                    sha256: hash(&content),
                    bytes: content.len() as u64,
                    executable,
                });
            }
            if let Entry::Static { path } = &manifest.entry {
                if manifest.build.is_empty() && !files.iter().any(|f| &f.path == path) {
                    return Err(AppError::Invalid(format!(
                        "static entry missing from package: {path}"
                    )));
                }
            }
            let release = AppRelease {
                app_id: record.app_id.clone(),
                revision,
                digest: digest(&manifest, &files)?,
                manifest: manifest.clone(),
                files,
                created_at_ms: now(),
                market_origin: request
                    .market_origin
                    .clone()
                    .or_else(|| record.market_origin.clone()),
            };
            atomic_json(&stage.join("release.json"), &release)?;
            Ok::<_, AppError>(release)
        })();
        let release = match staged {
            Ok(r) => r,
            Err(e) => {
                let _ = fs::remove_dir_all(&stage);
                return Err(e);
            }
        };
        let app = self.app_path(&record.app_id)?;
        let releases = app.join("releases");
        ensure_directory(&releases)?;
        ensure_directory(&app.join("data"))?;
        ensure_directory(&app.join("runs"))?;
        let final_path = releases.join(revision.to_string());
        if final_path.exists() {
            // An interrupted publication was never indexed. Preserve its content
            // in the transaction archive before retrying the same generation.
            fs::rename(
                &final_path,
                self.root
                    .join("transactions")
                    .join(format!("orphan-{}", id())),
            )?;
        }
        fs::rename(&stage, &final_path)?;
        #[cfg(unix)]
        File::open(&releases)?.sync_all()?;
        record.latest_revision = revision;
        record.name = manifest.name.clone();
        record.summary = manifest.summary.clone();
        record.updated_at_ms = now();
        if let Some(authoring) = request.authoring.or_else(|| {
            pending_authoring
                .as_ref()
                .map(|(_, authoring)| authoring.clone())
        }) {
            validate_authoring_owner(&authoring, &record.app_id)?;
            if record.authoring.is_none() || record.latest_revision == 1 {
                record.authoring = Some(authoring_with_owner(authoring, &record.app_id));
            }
        }
        if record.market_origin.is_none() {
            record.market_origin = request.market_origin;
        }
        if revision == 1 {
            record.data_schema_version = manifest.schema_version();
        }
        self.save(&record)?;
        if let Some((path, _)) = pending_authoring {
            // The record is already committed. A stale pending file is safe:
            // source_is_bound prevents it from being adopted or overwritten.
            if fs::remove_file(&path).is_ok() {
                if let Some(parent) = path.parent() {
                    if let Ok(directory) = File::open(parent) {
                        let _ = directory.sync_all();
                    }
                }
            }
        }
        Ok(PublishResult { record, release })
    }
    /// Verify and load a release.
    pub fn verify_release(&self, app_id: &str, revision: u64) -> Result<AppRelease, AppError> {
        let release = self.release(app_id, revision)?;
        let root = self.source_path(app_id, revision)?;
        let mut actual = vec![];
        let mut paths = vec![];
        enumerate(&root, &root, &mut paths)?;
        paths.sort();
        if paths
            != release
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>()
        {
            return Err(AppError::Invalid("release source inventory changed".into()));
        }
        for f in &release.files {
            let data = bytes(&checked_file(&root, &f.path)?)?;
            if hash(&data) != f.sha256
                || data.len() as u64 != f.bytes
                || executable(&checked_file(&root, &f.path)?)? != f.executable
            {
                return Err(AppError::Invalid(
                    "immutable release content changed".into(),
                ));
            }
            actual.push(f.clone());
        }
        if digest(&release.manifest, &actual)? != release.digest {
            return Err(AppError::Invalid("release digest mismatch".into()));
        }
        Ok(release)
    }
    /// Activate a published release for the app.
    pub fn activate_release(&self, app_id: &str, revision: u64) -> Result<AppRecord, AppError> {
        let _lock = self.lock()?;
        let mut record = self.get(app_id)?;
        let release = self.verify_release(app_id, revision)?;
        if release.manifest.schema_version() != record.data_schema_version {
            return Err(AppError::DataMigrationRequired {
                current: record.data_schema_version,
                target: release.manifest.schema_version(),
            });
        }
        if !release
            .manifest
            .accepts_data_schema(record.data_schema_version)
        {
            return Err(AppError::DataIncompatible {
                current: record.data_schema_version,
                target: release.manifest.schema_version(),
            });
        }
        record.current_revision = revision;
        record.data_schema_version = release.manifest.schema_version();
        record.market_origin = release.market_origin;
        record.name = release.manifest.name;
        record.summary = release.manifest.summary;
        record.updated_at_ms = now();
        self.save(&record)?;
        Ok(record)
    }
    /// Grant execution consent for a release digest.
    pub fn grant_execution(&self, app_id: &str, digest_value: &str) -> Result<AppRecord, AppError> {
        let _lock = self.lock()?;
        let mut record = self.get(app_id)?;
        let release = self
            .releases(app_id)?
            .into_iter()
            .find(|r| r.digest == digest_value)
            .ok_or_else(|| AppError::NotFound("release digest".into()))?;
        self.verify_release(app_id, release.revision)?;
        if !record.execution_grants.iter().any(|d| d == digest_value) {
            record.execution_grants.push(digest_value.into());
        }
        record.updated_at_ms = now();
        self.save(&record)?;
        Ok(record)
    }
    /// Revoke the app execution grants.
    pub fn revoke_execution(&self, app_id: &str) -> Result<AppRecord, AppError> {
        let _lock = self.lock()?;
        let mut record = self.get(app_id)?;
        record.execution_grants.clear();
        record.updated_at_ms = now();
        self.save(&record)?;
        Ok(record)
    }
    /// Require execution consent for a release.
    pub fn require_execution_grant(&self, app_id: &str, revision: u64) -> Result<(), AppError> {
        let record = self.get(app_id)?;
        let release = self.verify_release(app_id, revision)?;
        if release.manifest.requires_execution()
            && !record.execution_grants.contains(&release.digest)
        {
            return Err(AppError::ConsentRequired(release.digest));
        }
        Ok(())
    }
    /// Prepare a release to run.
    pub fn prepare_run(&self, app_id: &str, revision: u64) -> Result<RunPreparation, AppError> {
        self.prepare_run_with_options(app_id, revision, false)
    }
    /// Prepare a release to run with explicit options.
    pub fn prepare_run_with_options(
        &self,
        app_id: &str,
        revision: u64,
        rebuild: bool,
    ) -> Result<RunPreparation, AppError> {
        let _lock = self.lock()?;
        let record = self.get(app_id)?;
        let release = self.verify_release(app_id, revision)?;
        self.require_execution_grant(app_id, revision)?;
        if release.manifest.schema_version() != record.data_schema_version {
            return Err(AppError::DataMigrationRequired {
                current: record.data_schema_version,
                target: release.manifest.schema_version(),
            });
        }
        if !release
            .manifest
            .accepts_data_schema(record.data_schema_version)
        {
            return Err(AppError::DataIncompatible {
                current: record.data_schema_version,
                target: release.manifest.schema_version(),
            });
        }
        let timestamp = now();
        let run = AppRun {
            app_id: app_id.into(),
            run_id: id(),
            revision,
            release_digest: release.digest.clone(),
            phase: RunPhase::Preparing,
            services: vec![],
            error: None,
            started_at_ms: timestamp,
            updated_at_ms: timestamp,
        };
        let source = self.source_path(app_id, revision)?;
        let run_path = self.run_path(app_id, &run.run_id)?;
        let cached = self
            .app_path(app_id)?
            .join("environments")
            .join(format!("{revision}.json"));
        let mut work_run_id = run.run_id.clone();
        let mut environment_prepared = false;
        if !rebuild && cached.exists() {
            let saved: serde_json::Value = read_json(&cached)?;
            if saved["releaseDigest"].as_str() != Some(&release.digest) {
                return Err(AppError::Invalid(
                    "prepared environment release mismatch; repair required".into(),
                ));
            }
            let previous = saved["sourceRunId"]
                .as_str()
                .ok_or_else(|| AppError::Invalid("invalid prepared environment".into()))?;
            valid_id(previous)?;
            let previous_work = self.run_path(app_id, previous)?.join("work");
            if previous_work.is_dir() {
                if fs::symlink_metadata(&previous_work)?
                    .file_type()
                    .is_symlink()
                    || previous_work.canonicalize()? != previous_work
                {
                    return Err(AppError::Invalid(
                        "prepared environment path changed; repair required".into(),
                    ));
                }
                work_run_id = previous.into();
                environment_prepared = true;
            }
        }
        let work = self.run_path(app_id, &work_run_id)?.join("work");
        let preparation_data = run_path.join("preparation-data");
        ensure_directory(&work)?;
        ensure_directory(&preparation_data)?;
        for f in release.files.iter().filter(|_| !environment_prepared) {
            atomic_bytes(
                &work.join(&f.path),
                &bytes(&checked_file(&source, &f.path)?)?,
            )?;
            apply_executable(&work.join(&f.path), f.executable)?;
        }
        atomic_json(&run_path.join("run.json"), &run)?;
        atomic_json(
            &run_path.join("environment-work.json"),
            &serde_json::json!({"sourceRunId":work_run_id}),
        )?;
        Ok(RunPreparation {
            record,
            release,
            run,
            source_path: source.to_string_lossy().into(),
            work_path: work.to_string_lossy().into(),
            data_path: self.data_path(app_id)?.to_string_lossy().into(),
            preparation_data_path: preparation_data.to_string_lossy().into(),
            environment_prepared,
        })
    }
    fn run_path(&self, app_id: &str, run_id: &str) -> Result<PathBuf, AppError> {
        valid_id(run_id)?;
        Ok(self.app_path(app_id)?.join("runs").join(run_id))
    }
    /// List runs for an app.
    pub fn runs(&self, app_id: &str) -> Result<Vec<AppRun>, AppError> {
        self.get(app_id)?;
        let root = self.app_path(app_id)?.join("runs");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut runs = vec![];
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                return Err(AppError::Invalid("symlink run".into()));
            }
            let path = entry.path().join("run.json");
            if path.exists() {
                let run: AppRun = read_json(&path)?;
                if run.app_id != app_id {
                    return Err(AppError::Invalid("run identity mismatch".into()));
                }
                runs.push(run);
            }
        }
        runs.sort_by_key(|r| r.started_at_ms);
        Ok(runs)
    }
    /// Persist an updated run record.
    pub fn update_run(&self, mut run: AppRun) -> Result<(), AppError> {
        let _lock = self.lock()?;
        self.get(&run.app_id)?;
        let path = self.run_path(&run.app_id, &run.run_id)?.join("run.json");
        let previous: AppRun = read_json(&path)?;
        if previous.app_id != run.app_id
            || previous.run_id != run.run_id
            || previous.revision != run.revision
            || previous.release_digest != run.release_digest
            || previous.started_at_ms != run.started_at_ms
        {
            return Err(AppError::Invalid("run identity is immutable".into()));
        }
        if !previous.phase.is_live() && run.phase.is_live() {
            return Err(AppError::Invalid(
                "terminated run cannot be resurrected; prepare a new run".into(),
            ));
        }
        let release = self.release(&run.app_id, run.revision)?;
        let mut names = BTreeSet::new();
        for service in &run.services {
            if service.port == 0
                || !names.insert(&service.name)
                || !release
                    .manifest
                    .processes
                    .iter()
                    .any(|p| p.name == service.name)
            {
                return Err(AppError::Invalid("invalid run service registration".into()));
            }
        }
        if run.phase == RunPhase::Running {
            self.require_execution_grant(&run.app_id, run.revision)?;
        }
        run.updated_at_ms = now();
        atomic_json(&path, &run)?;
        if run.phase == RunPhase::Running {
            let work: serde_json::Value = read_json(&path.with_file_name("environment-work.json"))?;
            let source = work["sourceRunId"]
                .as_str()
                .ok_or_else(|| AppError::Invalid("invalid environment owner".into()))?;
            valid_id(source)?;
            let directory = self.run_path(&run.app_id, source)?.join("work");
            if !directory.is_dir() || directory.canonicalize()? != directory {
                return Err(AppError::Invalid(
                    "prepared environment is unavailable".into(),
                ));
            }
            atomic_json(
                &self
                    .app_path(&run.app_id)?
                    .join("environments")
                    .join(format!("{}.json", run.revision)),
                &serde_json::json!({"sourceRunId":source,"releaseDigest":run.release_digest}),
            )?;
        }
        Ok(())
    }
    /// Append text to a run log.
    pub fn append_log(&self, app_id: &str, run_id: &str, text: &str) -> Result<(), AppError> {
        let _lock = self.lock()?;
        self.get(app_id)?;
        let path = self.run_path(app_id, run_id)?;
        let _: AppRun = read_json(&path.join("run.json"))?;
        let log = path.join("output.log");
        let mut old = if log.exists() { bytes(&log)? } else { vec![] };
        old.extend_from_slice(text.as_bytes());
        if old.len() as u64 > MAX_LOG_BYTES {
            old.drain(..old.len() - MAX_LOG_BYTES as usize);
        }
        atomic_bytes(&log, &old)
    }
    /// Read a run log.
    pub fn logs(&self, app_id: &str, run_id: &str) -> Result<String, AppError> {
        self.get(app_id)?;
        let path = self.run_path(app_id, run_id)?;
        let _: AppRun = read_json(&path.join("run.json"))?;
        let log = path.join("output.log");
        Ok(if log.exists() {
            String::from_utf8_lossy(&bytes(&log)?).into()
        } else {
            String::new()
        })
    }
    /// Only called after successfully acquiring the exclusive runtime lease.
    fn recover_interrupted_runs(&self) -> Result<usize, AppError> {
        let mut count = 0;
        for app in self.list()? {
            for mut run in self.runs(&app.app_id)? {
                if run.phase.is_live() {
                    run.phase = RunPhase::Interrupted;
                    run.services.clear();
                    run.error =
                        Some("Host process ended; use restart to create a fresh run".into());
                    self.update_run(run)?;
                    count += 1;
                }
            }
        }
        Ok(count)
    }
    /// Mark an app removed.
    pub fn remove(&self, app_id: &str) -> Result<AppRecord, AppError> {
        let _lock = self.lock()?;
        let mut record = self.get(app_id)?;
        if self.runs(app_id)?.iter().any(|r| r.phase.is_live()) {
            return Err(AppError::Busy("stop app runs before removal".into()));
        }
        record.removed = true;
        record.execution_grants.clear();
        record.updated_at_ms = now();
        self.save(&record)?;
        Ok(record)
    }
    /// Bind an app to an authoring workspace.
    pub fn bind_authoring(
        &self,
        app_id: &str,
        authoring: AuthoringRef,
    ) -> Result<AppRecord, AppError> {
        validate_authoring_binding(&authoring)?;
        validate_authoring_owner(&authoring, app_id)?;
        self.store_authoring_pointer(app_id, authoring)
    }

    fn store_authoring_pointer(
        &self,
        app_id: &str,
        authoring: AuthoringRef,
    ) -> Result<AppRecord, AppError> {
        validate_authoring_source(&authoring)?;
        validate_authoring_owner(&authoring, app_id)?;
        let _lock = self.lock()?;
        let mut record = self.get(app_id)?;
        record.authoring = Some(authoring_with_owner(authoring, app_id));
        record.updated_at_ms = now();
        self.save(&record)?;
        Ok(record)
    }
    /// Inputs must come from the trusted host's existing session catalog and guest filesystem adapter.
    pub fn prepare_authoring(
        &self,
        app_id: &str,
        session_bindings: &BTreeMap<String, AuthoringSessionContext>,
        source_exists: bool,
    ) -> Result<AuthoringResolution, AppError> {
        let record = self.get(app_id)?;
        let Some(authoring) = record.authoring else {
            return Ok(AuthoringResolution {
                action: "sourceMissing".into(),
                source_directory: String::new(),
                authoring: None,
                authoring_session_id: None,
            });
        };
        if !source_exists {
            return Ok(AuthoringResolution {
                action: "sourceMissing".into(),
                source_directory: authoring.source_directory.clone(),
                authoring: Some(authoring),
                authoring_session_id: None,
            });
        }
        validate_authoring_source(&authoring)?;
        if !authoring_binding_complete(&authoring)
            || authoring.app_id.as_deref() != Some(record.app_id.as_str())
        {
            return Ok(AuthoringResolution {
                action: "rebindRequired".into(),
                source_directory: authoring.source_directory.clone(),
                authoring: Some(authoring),
                authoring_session_id: None,
            });
        }
        validate_authoring_binding(&authoring)?;
        let session_id = authoring
            .authoring_session_id
            .as_ref()
            .filter(|id| {
                session_bindings.get(*id).is_some_and(|context| {
                    context.workspace_id == authoring.workspace_id.as_deref().unwrap_or_default()
                        && context.source_directory == authoring.source_directory
                        && authoring.project_ref.as_ref() == Some(&context.project_ref)
                        && context.mode == "code"
                })
            })
            .cloned();
        Ok(AuthoringResolution {
            action: if session_id.is_some() {
                "resumeSession"
            } else {
                "createSession"
            }
            .into(),
            source_directory: authoring.source_directory.clone(),
            authoring: Some(authoring),
            authoring_session_id: session_id,
        })
    }
    /// Export a release to a destination.
    pub fn export(
        &self,
        app_id: &str,
        revision: u64,
        destination: &Path,
    ) -> Result<serde_json::Value, AppError> {
        let release = self.verify_release(app_id, revision)?;
        release.manifest.validate_public_export()?;
        let package = release.manifest.package.as_ref().ok_or_else(|| {
            AppError::Invalid("public export requires explicit package.files".into())
        })?;
        let mut allowed: BTreeSet<_> = package.files.iter().cloned().collect();
        allowed.insert(MANIFEST_FILE.into());
        if release
            .files
            .iter()
            .any(|f| !allowed.contains(&f.path) || excluded(&f.path))
        {
            return Err(AppError::Invalid(
                "release contains files outside public allowlist".into(),
            ));
        }
        if !destination.is_absolute() || destination.exists() {
            return Err(AppError::Invalid(
                "export destination must be a new absolute directory".into(),
            ));
        }
        ensure_directory(destination)?;
        let result = (|| {
            let source = self.source_path(app_id, revision)?;
            for f in &release.files {
                atomic_bytes(
                    &destination.join(&f.path),
                    &bytes(&checked_file(&source, &f.path)?)?,
                )?;
                apply_executable(&destination.join(&f.path), f.executable)?;
            }
            Ok::<_, AppError>(
                serde_json::json!({"path":destination,"digest":release.digest,"files":release.files}),
            )
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(destination);
        }
        result
    }
    /// List legacy app archives.
    pub fn legacy_archives(&self) -> Result<Vec<LegacyArchive>, AppError> {
        let legacy = self.config_home.join("local-apps");
        let mut result = vec![];
        for entry in fs::read_dir(legacy)? {
            let entry = entry?;
            if entry.file_name() == "v2" {
                continue;
            }
            result.push(LegacyArchive {
                path: entry.path().to_string_lossy().into(),
                kind: if entry.file_type()?.is_dir() {
                    "directory"
                } else {
                    "file"
                }
                .into(),
                status: "preservedNotMigrated".into(),
            });
        }
        result.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(result)
    }
    /// List legacy app entries.
    pub fn legacy_entries(&self) -> Result<Vec<LegacyEntry>, AppError> {
        let root = self.config_home.join("local-apps");
        let mut result = vec![];
        let index = root.join("index.json");
        if index.exists() {
            let value: serde_json::Value = read_json(&index)?;
            let entries = value
                .get("apps")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| AppError::Invalid("legacy index has no app map".into()))?;
            for (legacy_id, entry) in entries {
                valid_id(legacy_id)?;
                let revision = entry
                    .get("revisions")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|v| {
                        v.iter()
                            .filter_map(|r| r.get("rev").and_then(serde_json::Value::as_u64))
                            .max()
                    })
                    .ok_or_else(|| AppError::Invalid("legacy app has no revision".into()))?;
                let source = checked_file(&root, &format!("{legacy_id}/{revision}.html"))?;
                result.push(LegacyEntry {
                    legacy_id: legacy_id.clone(),
                    name: entry
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Imported app")
                        .into(),
                    summary: entry
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .into(),
                    revision,
                    source_path: source.to_string_lossy().into(),
                    status: "preservedNotMigrated".into(),
                });
            }
        }
        let desktop = root.join("desktop");
        if desktop.exists() {
            if fs::symlink_metadata(&desktop)?.file_type().is_symlink() {
                return Err(AppError::Invalid(
                    "legacy desktop archive is a symlink".into(),
                ));
            }
            for folder in fs::read_dir(&desktop)? {
                let folder = folder?;
                let name = folder.file_name().to_string_lossy().into_owned();
                if !folder.file_type()?.is_dir()
                    || !name.starts_with('a')
                    || valid_id(&name).is_err()
                {
                    continue;
                }
                let app_path = folder.path().join("app.json");
                if !app_path.exists() {
                    continue;
                }
                let record: serde_json::Value =
                    read_json(&checked_file(&desktop, &format!("{name}/app.json"))?)?;
                if record.get("id").and_then(serde_json::Value::as_str) != Some(name.as_str()) {
                    return Err(AppError::Invalid("legacy desktop identity mismatch".into()));
                }
                let revision = record
                    .get("revision")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|n| *n > 0)
                    .ok_or_else(|| AppError::Invalid("legacy desktop revision missing".into()))?;
                let source = folder.path().join("revisions").join(revision.to_string());
                let status = match checked_file(
                    &desktop,
                    &format!("{name}/revisions/{revision}/{MANIFEST_FILE}"),
                )
                .and_then(|path| read_json::<serde_json::Value>(&path))
                .and_then(upgrade_desktop_manifest)
                {
                    Ok(_) => "preservedNotMigrated",
                    Err(_) => "unsupportedPreserved",
                };
                result.push(LegacyEntry {
                    legacy_id: format!("desktop:{name}"),
                    name: record
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Archived desktop app")
                        .into(),
                    summary: record
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .into(),
                    revision,
                    source_path: source.to_string_lossy().into(),
                    status: status.into(),
                });
            }
        }
        result.sort_by(|a, b| a.name.cmp(&b.name).then(a.legacy_id.cmp(&b.legacy_id)));
        Ok(result)
    }
    fn import_legacy_desktop(
        &self,
        entry: &LegacyEntry,
        source_path: &Path,
        authoring: Option<AuthoringRef>,
    ) -> Result<PublishResult, AppError> {
        let source = Path::new(&entry.source_path);
        let manifest = upgrade_desktop_manifest(read_json(&checked_file(source, MANIFEST_FILE)?)?)?;
        let mut paths = vec![];
        enumerate(source, source, &mut paths)?;
        paths.sort();
        let mut total = 0u64;
        let mut copied = vec![];
        // Validate/read every file before creating a destination. No old runtime,
        // data, grants or browser profile lives inside a revision source tree.
        for path in &paths {
            let file = checked_file(source, path)?;
            let data = if path == MANIFEST_FILE {
                serde_json::to_vec_pretty(&manifest)?
            } else {
                bytes(&file)?
            };
            total = total.saturating_add(data.len() as u64);
            if total > MAX_RELEASE_BYTES {
                return Err(AppError::Invalid("legacy source exceeds 256 MiB".into()));
            }
            copied.push((path.clone(), data, executable(&file)?));
        }
        ensure_directory(source_path)?;
        for (path, data, execute) in copied {
            let destination = source_path.join(path);
            atomic_bytes(&destination, &data)?;
            apply_executable(&destination, execute)?;
        }
        self.publish(PublishRequest {
            app_id: None,
            expected_revision: 0,
            source_path: source_path.to_string_lossy().into(),
            authoring,
            market_origin: None,
        })
    }
    /// Explicit HTML import preserves the old package and browser storage references.
    /// Browser databases and visualization state are intentionally not copied into source.
    pub fn import_legacy(
        &self,
        legacy_id: &str,
        source_path: &Path,
        authoring: Option<AuthoringRef>,
    ) -> Result<PublishResult, AppError> {
        let entry = self
            .legacy_entries()?
            .into_iter()
            .find(|e| e.legacy_id == legacy_id)
            .ok_or_else(|| AppError::NotFound("legacy app".into()))?;
        if !source_path.is_absolute() || source_path.exists() {
            return Err(AppError::Invalid(
                "legacy import requires a new absolute source directory".into(),
            ));
        }
        if let Some(a) = &authoring {
            validate_authoring_source(a)?;
            if authoring_binding_complete(a) {
                validate_authoring_binding(a)?;
            }
        }
        if legacy_id.starts_with("desktop:") {
            return self.import_legacy_desktop(&entry, source_path, authoring);
        }
        let html = bytes(Path::new(&entry.source_path))?;
        let html_text = std::str::from_utf8(&html)
            .map_err(|_| AppError::Invalid("legacy document must be UTF-8".into()))?;
        if !html_text.to_ascii_lowercase().contains("<html")
            && !html_text.to_ascii_lowercase().contains("<!doctype html")
        {
            return Err(AppError::Invalid(
                "legacy asset is not a complete HTML document".into(),
            ));
        }
        let targets = ["android", "ios", "macos", "windows", "linux", "web"]
            .map(|platform| serde_json::json!({"platform":platform,"environment":"web"}));
        let manifest=Manifest::parse(&serde_json::json!({"schema":2,"name":entry.name,"summary":entry.summary,"entry":{"type":"static","path":"index.html"},"targets":targets,"package":{"files":["index.html"],"dependencyLocks":[]}}).to_string())?;
        ensure_directory(source_path)?;
        atomic_bytes(&source_path.join("index.html"), &html)?;
        atomic_json(&source_path.join(MANIFEST_FILE), &manifest)?;
        self.publish(PublishRequest {
            app_id: None,
            expected_revision: 0,
            source_path: source_path.to_string_lossy().into(),
            authoring,
            market_origin: None,
        })
    }
    fn copy_source(
        &self,
        app_id: &str,
        revision: u64,
        destination: &Path,
    ) -> Result<AppRelease, AppError> {
        let release = self.verify_release(app_id, revision)?;
        if !destination.is_absolute() || destination.exists() || destination.starts_with(&self.root)
        {
            return Err(AppError::Invalid(
                "source restoration needs a new directory outside the app library".into(),
            ));
        }
        ensure_directory(destination)?;
        let source = self.source_path(app_id, revision)?;
        for f in &release.files {
            atomic_bytes(
                &destination.join(&f.path),
                &bytes(&checked_file(&source, &f.path)?)?,
            )?;
            apply_executable(&destination.join(&f.path), f.executable)?;
        }
        Ok(release)
    }
    /// Restore an app authoring workspace.
    pub fn restore_authoring(
        &self,
        app_id: &str,
        revision: u64,
        source_path: &Path,
        authoring: AuthoringRef,
    ) -> Result<AppRecord, AppError> {
        validate_authoring_source(&authoring)?;
        if authoring_binding_complete(&authoring) {
            validate_authoring_binding(&authoring)?;
        }
        let record = self.get(app_id)?;
        if record.authoring.is_none()
            && record
                .market_origin
                .as_ref()
                .is_some_and(|o| o.remixed_from.is_none())
        {
            return Err(AppError::Invalid(
                "market installations require an authorized remix".into(),
            ));
        }
        self.copy_source(app_id, revision, source_path)?;
        if authoring_binding_complete(&authoring) {
            self.bind_authoring(app_id, authoring)
        } else {
            self.store_authoring_pointer(app_id, authoring)
        }
    }
    /// Create a new authoring workspace from an app release.
    pub fn remix(
        &self,
        app_id: &str,
        revision: u64,
        source_path: &Path,
        authoring: AuthoringRef,
        name: Option<String>,
    ) -> Result<PublishResult, AppError> {
        validate_authoring_source(&authoring)?;
        if authoring_binding_complete(&authoring) {
            validate_authoring_binding(&authoring)?;
        }
        let mut release = self.verify_release(app_id, revision)?;
        if !release
            .manifest
            .remix
            .as_ref()
            .is_some_and(|r| r.allowed && r.license.as_ref().is_some_and(|s| !s.trim().is_empty()))
        {
            return Err(AppError::Invalid(
                "this release does not authorize remixing".into(),
            ));
        }
        if let Some(name) = name {
            release.manifest.name = name;
            release.manifest.validate()?;
        }
        self.copy_source(app_id, revision, source_path)?;
        atomic_json(&source_path.join(MANIFEST_FILE), &release.manifest)?;
        let origin = release.market_origin.map(|mut origin| {
            origin.remixed_from = Some(origin.app_id.clone());
            origin
        });
        self.publish(PublishRequest {
            app_id: None,
            expected_revision: 0,
            source_path: source_path.to_string_lossy().into(),
            authoring: Some(authoring),
            market_origin: origin,
        })
    }
    /// Parse and execute a JSON library request.
    pub fn request_json(&self, request: &str) -> Result<serde_json::Value, AppError> {
        let request: LibraryRequest = serde_json::from_str(request)?;
        self.dispatch(request)
    }
    /// Execute a library request.
    pub fn dispatch(&self, request: LibraryRequest) -> Result<serde_json::Value, AppError> {
        use LibraryRequest::*;
        Ok(match request {
            List {} => serde_json::to_value(self.list()?)?,
            Get { app_id } => serde_json::to_value(self.get(&app_id)?)?,
            Publish { request } => serde_json::to_value(self.publish(request)?)?,
            Releases { app_id } => serde_json::to_value(self.releases(&app_id)?)?,
            GetRelease { app_id, revision } => {
                serde_json::to_value(self.release(&app_id, revision)?)?
            }
            Layout { app_id, revision } => serde_json::to_value(self.layout(
                &app_id,
                revision.unwrap_or(self.get(&app_id)?.current_revision),
            )?)?,
            Grant { app_id, digest } => {
                serde_json::to_value(self.grant_execution(&app_id, &digest)?)?
            }
            Revoke { app_id } => serde_json::to_value(self.revoke_execution(&app_id)?)?,
            ActivateRelease { app_id, revision } => {
                serde_json::to_value(self.activate_release(&app_id, revision)?)?
            }
            PrepareRun {
                app_id,
                revision,
                rebuild,
            } => serde_json::to_value(self.prepare_run_with_options(
                &app_id,
                revision.unwrap_or(self.get(&app_id)?.current_revision),
                rebuild,
            )?)?,
            UpdateRun { run } => {
                self.update_run(run)?;
                serde_json::json!({"updated":true})
            }
            Runs { app_id } => serde_json::to_value(self.runs(&app_id)?)?,
            Logs { app_id, run_id } => serde_json::to_value(self.logs(&app_id, &run_id)?)?,
            AppendLog {
                app_id,
                run_id,
                text,
            } => {
                self.append_log(&app_id, &run_id, &text)?;
                serde_json::json!({"appended":true})
            }
            CheckTarget {
                app_id,
                revision,
                target,
            } => {
                serde_json::json!({"compatible":self.release(&app_id,revision.unwrap_or(self.get(&app_id)?.current_revision))?.manifest.compatible_with(&target)?})
            }
            Remove { app_id } => serde_json::to_value(self.remove(&app_id)?)?,
            Export {
                app_id,
                revision,
                destination,
            } => self.export(&app_id, revision, Path::new(&destination))?,
            Import {
                source_path,
                authoring,
                market_origin,
            } => serde_json::to_value(self.publish(PublishRequest {
                app_id: None,
                expected_revision: 0,
                source_path,
                authoring,
                market_origin,
            })?)?,
            PrepareAuthoring {
                app_id,
                session_bindings,
                source_exists,
            } => serde_json::to_value(self.prepare_authoring(
                &app_id,
                &session_bindings,
                source_exists,
            )?)?,
            BindAuthoring { app_id, authoring } => {
                serde_json::to_value(self.bind_authoring(&app_id, authoring)?)?
            }
            RestoreAuthoring {
                app_id,
                revision,
                source_path,
                authoring,
            } => serde_json::to_value(self.restore_authoring(
                &app_id,
                revision.unwrap_or(self.get(&app_id)?.current_revision),
                Path::new(&source_path),
                authoring,
            )?)?,
            Remix {
                app_id,
                revision,
                source_path,
                authoring,
                name,
            } => serde_json::to_value(self.remix(
                &app_id,
                revision.unwrap_or(self.get(&app_id)?.current_revision),
                Path::new(&source_path),
                authoring,
                name,
            )?)?,
            LegacyArchives {} => serde_json::to_value(self.legacy_archives()?)?,
            LegacyEntries {} => serde_json::to_value(self.legacy_entries()?)?,
            ImportLegacy {
                legacy_id,
                source_path,
                authoring,
            } => serde_json::to_value(self.import_legacy(
                &legacy_id,
                Path::new(&source_path),
                authoring,
            )?)?,
        })
    }
}
