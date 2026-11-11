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

#[cfg(target_vendor = "apple")]
mod source_identity;

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RELEASE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeAuthoringReceipt {
    authoring: AuthoringRef,
    source_id: String,
    source_identity: rooted_fs::RootIdentity,
    session_mode: NativeAuthoringMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum NativeAuthoringMode {
    Code,
}

#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeAuthoringSource {
    source_id: String,
    source_directory: String,
    source_identity: rooted_fs::RootIdentity,
}

#[derive(Debug, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeSourceCreation {
    request_id: String,
    payload_sha256: String,
    source_id: String,
    source_directory: String,
    source_identity: rooted_fs::RootIdentity,
    target_app_id: Option<String>,
}
#[derive(Debug, Clone)]
/// Durable schema-2 app records, immutable releases, execution grants, and run history.
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

/// Derive one native publication request UUID from an acknowledged assistant
/// row and its owning session/tool use. Callers must first establish that the
/// row really was durably written by the matching session writer.
pub fn native_tool_publication_request_id(
    session: &str,
    assistant: &str,
    tool_use: &str,
) -> Result<String, AppError> {
    if !valid_uuid(session) || !valid_uuid(assistant) || tool_use.is_empty() {
        return Err(AppError::Invalid("invalid durable tool invocation".into()));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"lingxi-local-app-publish-invocation-v1\0");
    for part in [session, assistant, tool_use] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(bytes).to_string())
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
        p.eq_ignore_ascii_case(branding::DOT_DIR)
            || matches!(
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
            )
            || p == ".env"
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

#[cfg(test)]
mod excluded_tests {
    use super::excluded;

    #[test]
    fn current_config_directory_is_private_case_insensitively() {
        for directory in [
            branding::DOT_DIR.to_owned(),
            branding::DOT_DIR.to_ascii_uppercase(),
        ] {
            assert!(excluded(&format!("{directory}/private.json")));
            assert!(!excluded(&format!("{directory}-backup/index.html")));
        }
        assert!(excluded(".env.production"));
        assert!(!excluded("assets/index.html"));
    }
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

/// Fingerprint the exact normalized release that would be produced from a
/// validated inventory. The manifest is serialized as publication stores it.
pub fn fingerprint_manifest_inventory(
    manifest: &Manifest,
    files: &[ReleaseFile],
) -> Result<String, AppError> {
    let mut files = files.to_vec();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if files.windows(2).any(|pair| pair[0].path == pair[1].path) {
        return Err(AppError::Invalid("duplicate release file".into()));
    }
    let manifest_bytes = serde_json::to_vec_pretty(manifest)?;
    let manifest_file = files
        .iter_mut()
        .find(|file| file.path == MANIFEST_FILE)
        .ok_or_else(|| AppError::Invalid("release manifest is missing".into()))?;
    manifest_file.sha256 = hash(&manifest_bytes);
    manifest_file.bytes = manifest_bytes.len() as u64;
    manifest_file.executable = false;
    digest(manifest, &files)
}
fn validate_authoring_source(authoring: &AuthoringRef) -> Result<(), AppError> {
    if matches!(
        authoring.project_ref.as_ref(),
        Some(ProjectRef::DesktopPath { .. })
    ) {
        validate_native_absolute_path(&authoring.source_directory, "authoring sourceDirectory")
    } else {
        validate_canonical_absolute_path(&authoring.source_directory, "authoring sourceDirectory")
    }
}

fn validate_native_absolute_path(path: &str, label: &str) -> Result<(), AppError> {
    if path.contains('\0')
        || !Path::new(path).is_absolute()
        || Path::new(path).components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(AppError::Invalid(format!(
            "{label} must be an absolute canonical host path"
        )));
    }
    Ok(())
}

fn validate_native_or_guest_path(path: &str, label: &str) -> Result<(), AppError> {
    if validate_canonical_absolute_path(path, label).is_ok() {
        return Ok(());
    }
    validate_native_absolute_path(path, label)
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

/// Validate a Desktop host's already-selected physical source directory.
/// The native caller separately proves its Project and Code session owner.
#[cfg(any(windows, test))]
pub(super) fn windows_source_key(source: &str) -> String {
    let value = source.replace('/', "\\");
    let value = if value
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(r"\\?\UNC\"))
    {
        format!(r"\\{}", &value[8..])
    } else if value.starts_with(r"\\?\") {
        value[4..].to_owned()
    } else {
        value
    };
    value.to_lowercase()
}

fn native_canonical_matches(canonical: &Path, supplied: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_source_key(&canonical.to_string_lossy())
            == windows_source_key(&supplied.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        canonical == supplied
    }
}

/// Validate a canonical real authoring directory and capture its filesystem identity.
pub fn native_source_identity(source: &str) -> Result<rooted_fs::RootIdentity, AppError> {
    validate_native_absolute_path(source, "authoring sourceDirectory")?;
    if source.contains("//") || source.ends_with('/') {
        return Err(AppError::Invalid("invalid authoring source".into()));
    }
    let path = Path::new(source);
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || !native_canonical_matches(&fs::canonicalize(path)?, path)
    {
        return Err(AppError::Invalid(
            "authoring source must be a canonical real directory".into(),
        ));
    }
    let identity = rooted_fs::root_identity(path)
        .map_err(|e| AppError::Invalid(format!("invalid authoring source: {e}")))?;
    if !native_canonical_matches(&fs::canonicalize(path)?, path)
        || rooted_fs::root_identity(path).ok().as_ref() != Some(&identity)
    {
        return Err(AppError::Invalid(
            "authoring source changed during validation".into(),
        ));
    }
    rooted_fs::persistent_root_identity_pinned(path, &identity)
        .map_err(|error| AppError::Invalid(format!("invalid durable authoring source: {error}")))
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
            validate_native_absolute_path(path, "desktop projectRef.path")?;
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
impl AppLibrary {
    /// Open or create the device library below an absolute configuration directory.
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
        let library = Self { config_home, root };
        #[cfg(target_vendor = "apple")]
        source_identity::upgrade_current_sources(&library)?;
        Ok(library)
    }
    /// Return the canonical library directory used for records and immutable releases.
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
    /// Return the brand workspace directory reserved for authoring source trees.
    pub fn default_authoring_root(&self) -> PathBuf {
        branding::workspace_home(&self.config_home).join("apps")
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
    /// Read an app record, rejecting removed apps and mismatched stored identities.
    pub fn get(&self, app_id: &str) -> Result<AppRecord, AppError> {
        let record = self.get_any(app_id)?;
        if record.removed {
            return Err(AppError::NotFound(app_id.into()));
        }
        Ok(record)
    }
    /// List nonremoved app records from real library directories.
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
    fn native_authoring_receipt_name(source: &str, project: &ProjectRef, session: &str) -> String {
        let project = serde_json::to_string(project).expect("ProjectRef serializes");
        format!(
            "owner-{}.json",
            hash(format!("{source}\0{project}\0{session}").as_bytes())
        )
    }

    fn native_authoring_source_name(source: &str) -> String {
        #[cfg(windows)]
        let source = windows_source_key(source);
        #[cfg(not(windows))]
        let source = source.to_owned();
        format!("source-{}.json", hash(source.as_bytes()))
    }

    /// Issue or recover a native source proof by a client-persisted request
    /// UUID. Source proof and request index are separate durable writes: if
    /// interrupted between them, an unacknowledged orphan may remain.
    pub fn create_native_authoring_source<F, V>(
        &self,
        request_id: &str,
        payload: &serde_json::Value,
        target_app_id: Option<&str>,
        expected_source_id: Option<&str>,
        create: F,
        validate: V,
    ) -> Result<(String, String), AppError>
    where
        F: FnOnce() -> Result<String, AppError>,
        V: Fn(&str) -> Result<rooted_fs::RootIdentity, AppError>,
    {
        if !valid_uuid(request_id) {
            return Err(AppError::Invalid("invalid source requestId".into()));
        }
        let digest = hash(&serde_json::to_vec(payload)?);
        let _lock = self.lock()?;
        let root = self.root.join("pending-authoring");
        let pin = rooted_fs::root_identity(&root)
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
        let index_name = format!("creation-{request_id}.json");
        let index = rooted_fs::read_file_single_link_checked(&root, Path::new(&index_name))
            .map_err(|e| AppError::Invalid(format!("invalid source creation receipt: {e}")))?;
        if !index.matches_root(Some(&pin)) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        if let Some(content) = index.content {
            let creation: NativeSourceCreation = serde_json::from_str(&content)?;
            if creation.request_id != request_id
                || creation.payload_sha256 != digest
                || creation.target_app_id.as_deref() != target_app_id
            {
                return Err(AppError::Invalid(
                    "source requestId payload conflict".into(),
                ));
            }
            let proof = self
                .read_native_authoring_source(&root, &pin, &creation.source_directory)?
                .ok_or_else(|| AppError::Invalid("source creation proof is missing".into()))?;
            if proof.source_id != creation.source_id
                || expected_source_id.is_some_and(|id| id != proof.source_id)
                || proof.source_identity != creation.source_identity
                || validate(&creation.source_directory)? != creation.source_identity
            {
                return Err(AppError::Invalid("created source identity changed".into()));
            }
            return Ok((creation.source_id, creation.source_directory));
        }
        let source = create()?;
        let identity = validate(&source)?;
        // A Windows filesystem can expose the same directory through distinct
        // spellings that do not share even Unicode lowercase bytes. Never issue
        // a second source identity for an already-registered physical root.
        #[cfg(windows)]
        if self
            .read_native_authoring_source(&root, &pin, &source)?
            .is_none()
            && rooted_fs::root_identity(Path::new(&source)).ok() == Some(identity)
            && self.native_source_identity_registered(&root, &pin, &[identity])?
        {
            return Err(AppError::Invalid(
                "authoring source is already registered under another path".into(),
            ));
        }
        let source_id =
            if let Some(proof) = self.read_native_authoring_source(&root, &pin, &source)? {
                if proof.source_identity != identity
                    || expected_source_id.is_some_and(|id| id != proof.source_id)
                {
                    return Err(AppError::Invalid(
                        "authoring source identity mismatch".into(),
                    ));
                }
                proof.source_id
            } else {
                if expected_source_id.is_some() {
                    return Err(AppError::NotFound("authoring source".into()));
                }
                let source_id = Uuid::new_v4().to_string();
                let proof = NativeAuthoringSource {
                    source_id: source_id.clone(),
                    source_directory: source.clone(),
                    source_identity: identity,
                };
                let options = rooted_fs::AtomicWriteOptions {
                    overwrite: false,
                    create_parents: false,
                    ..Default::default()
                };
                rooted_fs::atomic_write_pinned(
                    &root,
                    Path::new(&Self::native_authoring_source_name(&source)),
                    &serde_json::to_vec_pretty(&proof)?,
                    options,
                    &pin,
                )
                .map_err(|e| {
                    AppError::Invalid(format!("cannot persist authoring source receipt: {e}"))
                })?;
                source_id
            };
        if let Some(app_id) = target_app_id {
            valid_id(app_id)?;
            self.get(app_id)?;
            if self
                .find_native_source_owner_locked(&source_id)?
                .as_deref()
                .is_some_and(|owner| owner != app_id)
            {
                return Err(AppError::Invalid(
                    "authoring source belongs to another application".into(),
                ));
            }
        }
        if validate(&source)? != identity {
            return Err(AppError::Invalid(
                "authoring source changed during creation".into(),
            ));
        }
        let creation = NativeSourceCreation {
            request_id: request_id.into(),
            payload_sha256: digest,
            source_id: source_id.clone(),
            source_directory: source.clone(),
            source_identity: identity,
            target_app_id: target_app_id.map(str::to_owned),
        };
        let options = rooted_fs::AtomicWriteOptions {
            overwrite: false,
            create_parents: false,
            ..Default::default()
        };
        rooted_fs::atomic_write_pinned(
            &root,
            Path::new(&index_name),
            &serde_json::to_vec_pretty(&creation)?,
            options,
            &pin,
        )
        .map_err(|e| AppError::Invalid(format!("cannot persist source creation receipt: {e}")))?;
        Ok((source_id, source))
    }

    fn read_native_authoring_source(
        &self,
        root: &Path,
        pin: &rooted_fs::RootIdentity,
        source: &str,
    ) -> Result<Option<NativeAuthoringSource>, AppError> {
        validate_native_or_guest_path(source, "authoring sourceDirectory")?;
        if source.contains("//") || source.ends_with('/') {
            return Err(AppError::Invalid("invalid authoring source".into()));
        }
        let name = Self::native_authoring_source_name(source);
        let snapshot = rooted_fs::read_file_single_link_checked(root, Path::new(&name))
            .map_err(|e| AppError::Invalid(format!("invalid authoring source receipt: {e}")))?;
        if !snapshot.matches_root(Some(pin)) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        let Some(content) = snapshot.content else {
            return Ok(None);
        };
        let receipt: NativeAuthoringSource = serde_json::from_str(&content)?;
        #[cfg(windows)]
        let source_matches =
            windows_source_key(&receipt.source_directory) == windows_source_key(source);
        #[cfg(not(windows))]
        let source_matches = receipt.source_directory == source;
        if !source_matches || !valid_uuid(&receipt.source_id) {
            return Err(AppError::Invalid(
                "authoring source receipt identity mismatch".into(),
            ));
        }
        Ok(Some(receipt))
    }

    #[cfg(windows)]
    fn native_source_identity_registered(
        &self,
        root: &Path,
        pin: &rooted_fs::RootIdentity,
        identities: &[rooted_fs::RootIdentity],
    ) -> Result<bool, AppError> {
        let mut found = false;
        for entry in fs::read_dir(root)? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| AppError::Invalid("invalid authoring receipt filename".into()))?;
            if !name.starts_with("source-") || !name.ends_with(".json") {
                continue;
            }
            let snapshot = rooted_fs::read_file_single_link_checked(root, Path::new(&name))
                .map_err(|e| AppError::Invalid(format!("invalid authoring source receipt: {e}")))?;
            if !snapshot.matches_root(Some(pin)) {
                return Err(AppError::Invalid("authoring receipt root changed".into()));
            }
            let content = snapshot
                .content
                .ok_or_else(|| AppError::Invalid("authoring source receipt disappeared".into()))?;
            let receipt: NativeAuthoringSource = serde_json::from_str(&content)?;
            if !valid_uuid(&receipt.source_id)
                || name != Self::native_authoring_source_name(&receipt.source_directory)
            {
                return Err(AppError::Invalid(
                    "authoring source receipt identity mismatch".into(),
                ));
            }
            found |= identities.contains(&receipt.source_identity);
        }
        if rooted_fs::root_identity(root).ok().as_ref() != Some(pin) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        Ok(found)
    }

    /// Resolve an existing source proof; `expected_id` is required for cold
    /// provisional recovery, while a trusted published record may discover it.
    pub fn prepare_native_authoring_source<F>(
        &self,
        source: &str,
        expected_id: Option<&str>,
        validate: F,
    ) -> Result<String, AppError>
    where
        F: FnOnce() -> Result<rooted_fs::RootIdentity, AppError>,
    {
        if expected_id.is_some_and(|id| !valid_uuid(id)) {
            return Err(AppError::Invalid("invalid authoring sourceId".into()));
        }
        let _lock = self.lock()?;
        let root = self.root.join("pending-authoring");
        let pin = rooted_fs::root_identity(&root)
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
        let receipt = self
            .read_native_authoring_source(&root, &pin, source)?
            .ok_or_else(|| AppError::NotFound("authoring source".into()))?;
        if expected_id.is_some_and(|id| id != receipt.source_id)
            || validate()? != receipt.source_identity
        {
            return Err(AppError::Invalid(
                "authoring source identity mismatch".into(),
            ));
        }
        if rooted_fs::root_identity(&root).ok().as_ref() != Some(&pin) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        Ok(receipt.source_id)
    }

    /// Native-only lookup of a source proof and its durable app affinity.
    pub fn find_native_authoring_source<F>(
        &self,
        source: &str,
        validate: F,
    ) -> Result<(String, Option<String>), AppError>
    where
        F: FnOnce() -> Result<rooted_fs::RootIdentity, AppError>,
    {
        let _lock = self.lock()?;
        let root = self.root.join("pending-authoring");
        let pin = rooted_fs::root_identity(&root)
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
        let proof = self
            .read_native_authoring_source(&root, &pin, source)?
            .ok_or_else(|| AppError::NotFound("authoring source".into()))?;
        if validate()? != proof.source_identity {
            return Err(AppError::Invalid(
                "authoring source identity mismatch".into(),
            ));
        }
        let owner = self.find_native_source_owner_locked(&proof.source_id)?;
        Ok((proof.source_id, owner))
    }

    fn read_native_authoring_receipt(
        &self,
        root: &Path,
        pin: &rooted_fs::RootIdentity,
        source: &str,
        project: &ProjectRef,
        session: &str,
    ) -> Result<Option<NativeAuthoringReceipt>, AppError> {
        validate_native_or_guest_path(source, "authoring sourceDirectory")?;
        let valid_project = match project {
            ProjectRef::MobileProject { project_id } => valid_uuid(project_id),
            ProjectRef::DesktopPath { path } => path == source,
        };
        if source.contains("//") || source.ends_with('/') || !valid_project || !valid_uuid(session)
        {
            return Err(AppError::Invalid("invalid mobile authoring lookup".into()));
        }
        let name = Self::native_authoring_receipt_name(source, project, session);
        let snapshot = rooted_fs::read_file_single_link_checked(root, Path::new(&name))
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt: {e}")))?;
        if !snapshot.matches_root(Some(pin)) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        let Some(content) = snapshot.content else {
            return Ok(None);
        };
        let receipt: NativeAuthoringReceipt = serde_json::from_str(&content)?;
        validate_authoring_binding(&receipt.authoring)?;
        if receipt.authoring.source_directory != source
            || receipt.authoring.project_ref.as_ref() != Some(project)
            || receipt.authoring.authoring_session_id.as_deref() != Some(session)
        {
            return Err(AppError::Invalid(
                "authoring receipt identity mismatch".into(),
            ));
        }
        Ok(Some(receipt))
    }

    /// Caller holds the library lock; ambiguous affinity never selects a winner.
    fn find_native_source_owner_locked(&self, source_id: &str) -> Result<Option<String>, AppError> {
        let root = self.root.join("apps");
        let pin = rooted_fs::root_identity(&root)
            .map_err(|e| AppError::Invalid(format!("invalid app records root: {e}")))?;
        let mut owner = None;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(AppError::Invalid("symlink app directory".into()));
            }
            if !kind.is_dir() {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| AppError::Invalid("invalid app directory name".into()))?;
            valid_id(&name)?;
            let snapshot = rooted_fs::read_file_single_link_checked(
                &root,
                &Path::new(&name).join("record.json"),
            )
            .map_err(|e| AppError::Invalid(format!("invalid app record: {e}")))?;
            if !snapshot.matches_root(Some(&pin)) {
                return Err(AppError::Invalid("app records root changed".into()));
            }
            let Some(content) = snapshot.content else {
                continue;
            };
            let record: AppRecord = serde_json::from_str(&content)?;
            if record.app_id != name {
                return Err(AppError::Invalid("record identity mismatch".into()));
            }
            // A soft-deleted App retains its source authority. Reusing its
            // files for another App requires a new native source proof.
            if record.authoring_source_ids.iter().any(|id| id == source_id)
                && owner.replace(name).is_some()
            {
                return Err(AppError::Invalid(
                    "ambiguous authoring source affinity".into(),
                ));
            }
        }
        if rooted_fs::root_identity(&root).ok().as_ref() != Some(&pin) {
            return Err(AppError::Invalid("app records root changed".into()));
        }
        Ok(owner)
    }

    /// Register a native-confirmed immutable owner. The current publication
    /// pointer is separate from this receipt: changing it never revokes older
    /// owners. Only the trusted host may call this after authenticating the
    /// Project and Code session; source validation runs under the library lock.
    pub fn register_native_authoring<F>(
        &self,
        authoring: AuthoringRef,
        source_id: &str,
        request_id: &str,
        set_current: bool,
        mut validate_source: F,
    ) -> Result<AuthoringRef, AppError>
    where
        F: FnMut(&AuthoringRef) -> Result<rooted_fs::RootIdentity, AppError>,
    {
        validate_authoring_binding(&authoring)?;
        let project = authoring.project_ref.as_ref().unwrap();
        if matches!(project, ProjectRef::DesktopPath { path } if path != &authoring.source_directory)
        {
            return Err(AppError::Invalid(
                "desktop authoring Project must match its source".into(),
            ));
        }
        let session = authoring.authoring_session_id.as_deref().unwrap();
        let _lock = self.lock()?;
        let source_identity = validate_source(&authoring)?;
        let pending_root = self.root.join("pending-authoring");
        let pending_pin = rooted_fs::root_identity(&pending_root)
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
        let source_receipt = self
            .read_native_authoring_source(&pending_root, &pending_pin, &authoring.source_directory)?
            .ok_or_else(|| AppError::NotFound("authoring source".into()))?;
        if source_receipt.source_id != source_id
            || source_receipt.source_identity != source_identity
        {
            return Err(AppError::Invalid("authoring source proof mismatch".into()));
        }
        if !valid_uuid(request_id) {
            return Err(AppError::Invalid("invalid source requestId".into()));
        }
        let index = rooted_fs::read_file_single_link_checked(
            &pending_root,
            Path::new(&format!("creation-{request_id}.json")),
        )
        .map_err(|e| AppError::Invalid(format!("invalid source creation receipt: {e}")))?;
        if !index.matches_root(Some(&pending_pin)) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        let creation: NativeSourceCreation = serde_json::from_str(
            &index
                .content
                .ok_or_else(|| AppError::NotFound("authoring source intent".into()))?,
        )?;
        if creation.request_id != request_id
            || creation.source_id != source_id
            || creation.source_directory != authoring.source_directory
            || creation.source_identity != source_identity
            || creation
                .target_app_id
                .as_deref()
                .is_some_and(|id| Some(id) != authoring.app_id.as_deref())
        {
            return Err(AppError::Invalid("authoring source intent mismatch".into()));
        }
        let existing = self.read_native_authoring_receipt(
            &pending_root,
            &pending_pin,
            &authoring.source_directory,
            project,
            session,
        )?;
        if let Some(receipt) = &existing {
            let mut supplied = authoring.clone();
            let mut saved = receipt.authoring.clone();
            supplied.app_id = None;
            saved.app_id = None;
            if supplied != saved
                || receipt.source_identity != source_identity
                || receipt.source_id != source_id
            {
                return Err(AppError::Invalid(
                    "authoring owner receipt conflicts with the existing binding".into(),
                ));
            }
        }
        let apps_root = self.root.join("apps");
        let apps_pin = rooted_fs::root_identity(&apps_root)
            .map_err(|e| AppError::Invalid(format!("invalid app records root: {e}")))?;
        let source_owner = self.find_native_source_owner_locked(source_id)?;
        if authoring.app_id.is_none() && source_owner.is_some() {
            if let Some(receipt) = existing.as_ref() {
                if validate_source(&authoring)? != source_identity {
                    return Err(AppError::Invalid(
                        "authoring source changed during registration".into(),
                    ));
                }
                return Ok(receipt.authoring.clone());
            }
        }
        if source_owner
            .as_deref()
            .is_some_and(|id| Some(id) != authoring.app_id.as_deref())
        {
            return Err(AppError::Invalid(
                "authoring source belongs to another published application".into(),
            ));
        }
        if source_owner.is_none()
            && authoring.app_id.is_some()
            && creation.target_app_id.as_deref() != authoring.app_id.as_deref()
        {
            return Err(AppError::Invalid(
                "authoring source has no trusted app intent".into(),
            ));
        }
        let current_record = if let Some(app_id) = authoring.app_id.as_deref() {
            valid_id(app_id)?;
            let relative = Path::new(app_id).join("record.json");
            let snapshot = rooted_fs::read_file_single_link_checked(&apps_root, &relative)
                .map_err(|e| AppError::Invalid(format!("invalid app record: {e}")))?;
            if !snapshot.matches_root(Some(&apps_pin)) {
                return Err(AppError::Invalid("app records root changed".into()));
            }
            let content = snapshot
                .content
                .ok_or_else(|| AppError::NotFound(app_id.into()))?;
            let record: AppRecord = serde_json::from_str(&content)?;
            if record.removed || record.app_id != app_id {
                return Err(AppError::NotFound(app_id.into()));
            }
            Some((relative, record))
        } else {
            None
        };
        // Publish the immutable receipt before changing any current pointer.
        // A later failure can leave a valid receipt, but cannot leave current
        // pointing to an owner whose receipt was never committed.
        let saved = if let Some(receipt) = existing {
            receipt.authoring
        } else {
            let name =
                Self::native_authoring_receipt_name(&authoring.source_directory, project, session);
            let receipt = NativeAuthoringReceipt {
                authoring: authoring.clone(),
                source_id: source_id.into(),
                source_identity,
                session_mode: NativeAuthoringMode::Code,
            };
            let options = rooted_fs::AtomicWriteOptions {
                overwrite: false,
                create_parents: false,
                ..Default::default()
            };
            rooted_fs::atomic_write_pinned(
                &pending_root,
                Path::new(&name),
                &serde_json::to_vec_pretty(&receipt)?,
                options,
                &pending_pin,
            )
            .map_err(|e| AppError::Invalid(format!("cannot persist authoring receipt: {e}")))?;
            receipt.authoring
        };
        if validate_source(&authoring)? != source_identity {
            return Err(AppError::Invalid(
                "authoring source changed during registration".into(),
            ));
        }
        if set_current || (source_owner.is_none() && authoring.app_id.is_some()) {
            let options = rooted_fs::AtomicWriteOptions {
                create_parents: false,
                ..Default::default()
            };
            if let Some((relative, mut record)) = current_record {
                let app_id = authoring.app_id.as_deref().unwrap();
                if !record.authoring_source_ids.iter().any(|id| id == source_id) {
                    record.authoring_source_ids.push(source_id.into());
                }
                if set_current {
                    record.authoring = Some(authoring_with_owner(authoring.clone(), app_id));
                }
                record.updated_at_ms = now();
                rooted_fs::atomic_write_pinned(
                    &apps_root,
                    &relative,
                    &serde_json::to_vec_pretty(&record)?,
                    options,
                    &apps_pin,
                )
                .map_err(|e| AppError::Invalid(format!("cannot update authoring pointer: {e}")))?;
            } else if set_current {
                let name = format!("{}.json", hash(authoring.source_directory.as_bytes()));
                rooted_fs::atomic_write_pinned(
                    &pending_root,
                    Path::new(&name),
                    &serde_json::to_vec_pretty(&authoring)?,
                    options,
                    &pending_pin,
                )
                .map_err(|e| AppError::Invalid(format!("cannot update pending authoring: {e}")))?;
            }
        }
        Ok(saved)
    }

    /// Read an immutable native owner receipt. A current AppRecord pointer is
    /// never treated as an authorization grant.
    pub fn resolve_native_authoring<F>(
        &self,
        source: &str,
        project: &ProjectRef,
        session: &str,
        validate_source: F,
    ) -> Result<AuthoringRef, AppError>
    where
        F: FnOnce(&AuthoringRef) -> Result<rooted_fs::RootIdentity, AppError>,
    {
        self.resolve_native_authoring_with_source_id(source, project, session, validate_source)
            .map(|(_, authoring, _)| authoring)
    }

    /// Validate native source and authoring receipts, returning source ID, binding, and optional owning app ID.
    pub fn resolve_native_authoring_with_source_id<F>(
        &self,
        source: &str,
        project: &ProjectRef,
        session: &str,
        validate_source: F,
    ) -> Result<(String, AuthoringRef, Option<String>), AppError>
    where
        F: FnOnce(&AuthoringRef) -> Result<rooted_fs::RootIdentity, AppError>,
    {
        let _lock = self.lock()?;
        let root = self.root.join("pending-authoring");
        let pin = rooted_fs::root_identity(&root)
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
        let receipt = self
            .read_native_authoring_receipt(&root, &pin, source, project, session)?
            .ok_or_else(|| AppError::NotFound("authoring binding".into()))?;
        let source_receipt = self
            .read_native_authoring_source(&root, &pin, source)?
            .ok_or_else(|| AppError::NotFound("authoring source".into()))?;
        if source_receipt.source_id != receipt.source_id
            || source_receipt.source_identity != receipt.source_identity
        {
            return Err(AppError::Invalid("authoring source proof mismatch".into()));
        }
        if validate_source(&receipt.authoring)? != receipt.source_identity {
            return Err(AppError::Invalid(
                "authoring source identity changed".into(),
            ));
        }
        if rooted_fs::root_identity(&root).ok().as_ref() != Some(&pin) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        let owner = self.find_native_source_owner_locked(&receipt.source_id)?;
        Ok((receipt.source_id, receipt.authoring, owner))
    }
    /// Derive the source directory for a positive revision without verifying that it exists.
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
    /// Return the retained data directory of an existing nonremoved app.
    pub fn data_path(&self, app_id: &str) -> Result<PathBuf, AppError> {
        self.get(app_id)?;
        Ok(self.app_path(app_id)?.join("data"))
    }
    /// Read a release, validating its app identity, revision, and manifest.
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
    /// Read all published revisions in ascending revision order.
    pub fn releases(&self, app_id: &str) -> Result<Vec<AppRelease>, AppError> {
        let record = self.get(app_id)?;
        (1..=record.latest_revision)
            .map(|revision| self.release(app_id, revision))
            .collect()
    }
    /// Resolve the source, data, releases, and runs directories for a validated release.
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
    /// Publish an unbound source tree; authoring ownership requires the native receipt APIs.
    pub fn publish(&self, request: PublishRequest) -> Result<PublishResult, AppError> {
        self.publish_unbound(request)
    }

    /// Compute the exact release digest for a staged source without publishing.
    /// A later publication checks it again after reading the actual files.
    pub fn fingerprint_publication(&self, source: &Path) -> Result<String, AppError> {
        if !source.is_absolute() {
            return Err(AppError::Invalid("sourcePath must be absolute".into()));
        }
        let metadata = fs::symlink_metadata(source)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
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
            let mut paths = Vec::new();
            enumerate(&source, &source, &mut paths)?;
            paths
        };
        if !paths.iter().any(|path| path == MANIFEST_FILE) {
            paths.push(MANIFEST_FILE.into());
        }
        paths.sort();
        paths.dedup();
        if paths.len() > MAX_FILES {
            return Err(AppError::Invalid("too many package files".into()));
        }
        let mut files = Vec::new();
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
            files.push(ReleaseFile {
                path: path.clone(),
                sha256: hash(&content),
                bytes: content.len() as u64,
                executable: path != MANIFEST_FILE && executable(&checked_file(&source, path)?)?,
            });
        }
        if let Entry::Static { path } = &manifest.entry {
            if manifest.build.is_empty() && !files.iter().any(|file| &file.path == path) {
                return Err(AppError::Invalid(format!(
                    "static entry missing from package: {path}"
                )));
            }
        }
        digest(&manifest, &files)
    }

    /// The host supplies the actual engine session, never model input. The
    /// source must have exactly one matching immutable native owner receipt.
    pub fn publish_with_native_authoring<F>(
        &self,
        request: PublishRequest,
        source: &str,
        session: &str,
        expected_project: Option<&ProjectRef>,
        expected_source_id: Option<&str>,
        set_current: bool,
        validate: F,
    ) -> Result<PublishResult, AppError>
    where
        F: Fn(&AuthoringRef) -> Result<rooted_fs::RootIdentity, AppError>,
    {
        validate_native_or_guest_path(source, "authoring sourceDirectory")?;
        if !valid_uuid(session)
            || request.authoring.is_some()
            || (request.app_id.is_none() && !set_current)
        {
            return Err(AppError::Invalid(
                "invalid mobile authoring publication".into(),
            ));
        }
        self.publish_inner(
            request,
            Some((
                source,
                session,
                expected_project,
                expected_source_id,
                set_current,
                &validate,
            )),
        )
    }

    /// Publish once for a durable native request UUID. The caller supplies the
    /// digest of the staged release, obtained from `fingerprint_publication`.
    /// A committed result is returned before the staging path is touched.
    pub fn publish_native_authoring_once<F>(
        &self,
        request_id: &str,
        staged_digest: &str,
        request: PublishRequest,
        source: &str,
        session: &str,
        expected_project: Option<&ProjectRef>,
        expected_source_id: Option<&str>,
        set_current: bool,
        validate: F,
    ) -> Result<PublishResult, AppError>
    where
        F: Fn(&AuthoringRef) -> Result<rooted_fs::RootIdentity, AppError>,
    {
        let payload_sha = Self::native_authoring_publication_payload(
            request_id,
            staged_digest,
            &request,
            source,
            session,
            expected_project,
            expected_source_id,
            set_current,
        )?;
        let _lock = self.lock()?;
        if let Some(result) =
            self.publication_replay_locked(request_id, &payload_sha, staged_digest)?
        {
            return Ok(result);
        }
        self.publish_locked(
            request,
            Some((
                source,
                session,
                expected_project,
                expected_source_id,
                set_current,
                &validate,
            )),
            Some((request_id, &payload_sha, staged_digest)),
            None,
        )
    }

    fn native_authoring_publication_payload(
        request_id: &str,
        staged_digest: &str,
        request: &PublishRequest,
        source: &str,
        session: &str,
        expected_project: Option<&ProjectRef>,
        expected_source_id: Option<&str>,
        set_current: bool,
    ) -> Result<String, AppError> {
        if !valid_uuid(request_id)
            || staged_digest.len() != 64
            || !staged_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || !valid_uuid(session)
            || request.authoring.is_some()
            || (request.app_id.is_none() && !set_current)
        {
            return Err(AppError::Invalid(
                "invalid native publication request".into(),
            ));
        }
        validate_native_or_guest_path(source, "authoring sourceDirectory")?;
        let payload = serde_json::json!({"appId":request.app_id,"expectedRevision":request.expected_revision,
            "marketOrigin":request.market_origin,"sourceDirectory":source,"sessionId":session,
            "projectRef":expected_project,"sourceId":expected_source_id,"setCurrent":set_current,
            "stagedDigest":staged_digest});
        Ok(hash(&serde_json::to_vec(&payload)?))
    }

    /// Verify a committed authoring publication without allowing a new write.
    pub fn replay_native_authoring_publication(
        &self,
        request_id: &str,
        staged_digest: &str,
        request: &PublishRequest,
        source: &str,
        session: &str,
        project: Option<&ProjectRef>,
        source_id: Option<&str>,
        set_current: bool,
    ) -> Result<PublishResult, AppError> {
        let payload = Self::native_authoring_publication_payload(
            request_id,
            staged_digest,
            request,
            source,
            session,
            project,
            source_id,
            set_current,
        )?;
        let _lock = self.lock()?;
        self.publication_replay_locked(request_id, &payload, staged_digest)?
            .ok_or_else(|| AppError::NotFound("committed publication".into()))
    }

    /// Publish an ordinary, unbound source once using a native request UUID.
    /// A registered authoring source or application must use its owner proof.
    pub fn publish_once(
        &self,
        request_id: &str,
        staged_digest: &str,
        request: PublishRequest,
        original_source: Option<&str>,
    ) -> Result<PublishResult, AppError> {
        let payload_sha = Self::ordinary_publication_payload(
            request_id,
            staged_digest,
            &request,
            original_source,
        )?;
        let _lock = self.lock()?;
        if let Some(result) =
            self.publication_replay_locked(request_id, &payload_sha, staged_digest)?
        {
            return Ok(result);
        }
        if let Some(source) = original_source {
            self.reject_registered_source_ancestors_locked(Path::new(source))?;
        }
        self.publish_locked(
            request,
            None,
            Some((request_id, &payload_sha, staged_digest)),
            None,
        )
    }

    fn ordinary_publication_payload(
        request_id: &str,
        staged_digest: &str,
        request: &PublishRequest,
        original_source: Option<&str>,
    ) -> Result<String, AppError> {
        if !valid_uuid(request_id)
            || staged_digest.len() != 64
            || !staged_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || request.authoring.is_some()
        {
            return Err(AppError::Invalid(
                "invalid native publication request".into(),
            ));
        }
        if let Some(source) = original_source {
            validate_native_or_guest_path(source, "sourceDirectory")?;
        }
        let payload = serde_json::json!({"kind":"ordinary","appId":request.app_id,
            "expectedRevision":request.expected_revision,"marketOrigin":request.market_origin,
            "sourceDirectory":original_source,"stagedDigest":staged_digest});
        Ok(hash(&serde_json::to_vec(&payload)?))
    }

    /// Verify a committed ordinary publication without initiating a write.
    pub fn replay_publication(
        &self,
        request_id: &str,
        staged_digest: &str,
        request: &PublishRequest,
        original_source: Option<&str>,
    ) -> Result<PublishResult, AppError> {
        let payload = Self::ordinary_publication_payload(
            request_id,
            staged_digest,
            request,
            original_source,
        )?;
        let _lock = self.lock()?;
        self.publication_replay_locked(request_id, &payload, staged_digest)?
            .ok_or_else(|| AppError::NotFound("committed publication".into()))
    }

    fn publication_replay_locked(
        &self,
        request_id: &str,
        payload_sha: &str,
        staged_digest: &str,
    ) -> Result<Option<PublishResult>, AppError> {
        let Some((receipt, result)) = self.find_native_publication_locked(request_id)? else {
            return Ok(None);
        };
        if receipt.payload_sha256 != payload_sha || result.release.digest != staged_digest {
            return Err(AppError::Invalid("publication requestId conflict".into()));
        }
        Ok(Some(result))
    }

    /// Inspect a committed native publication without initiating a write.
    pub fn native_authoring_publication(
        &self,
        request_id: &str,
    ) -> Result<Option<(AuthoringPublicationReceipt, PublishResult)>, AppError> {
        if !valid_uuid(request_id) {
            return Err(AppError::Invalid("invalid publication requestId".into()));
        }
        let _lock = self.lock()?;
        self.find_native_publication_locked(request_id)
    }

    fn remix_creation_locked(
        &self,
        request_id: &str,
        payload_sha: &str,
    ) -> Result<NativeSourceCreation, AppError> {
        if !valid_uuid(request_id) {
            return Err(AppError::Invalid("invalid source requestId".into()));
        }
        let root = self.root.join("pending-authoring");
        let pin = rooted_fs::root_identity(&root)
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
        let name = format!("creation-{request_id}.json");
        let snapshot = rooted_fs::read_file_single_link_checked(&root, Path::new(&name))
            .map_err(|e| AppError::Invalid(format!("invalid source creation receipt: {e}")))?;
        if !snapshot.matches_root(Some(&pin)) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        let creation: NativeSourceCreation = serde_json::from_str(
            &snapshot
                .content
                .ok_or_else(|| AppError::NotFound("source creation".into()))?,
        )?;
        if creation.request_id != request_id
            || creation.payload_sha256 != payload_sha
            || creation.target_app_id.is_some()
        {
            return Err(AppError::Invalid(
                "source requestId payload conflict".into(),
            ));
        }
        Ok(creation)
    }

    /// Look up a completed native remix before accessing its original input.
    /// The request payload must match the durable creation and publication.
    pub fn replay_native_remix(
        &self,
        request_id: &str,
        payload: &serde_json::Value,
    ) -> Result<Option<(String, String, PublishResult)>, AppError> {
        if !valid_uuid(request_id) {
            return Err(AppError::Invalid("invalid source requestId".into()));
        }
        let payload_sha = hash(&serde_json::to_vec(payload)?);
        let _lock = self.lock()?;
        self.replay_native_remix_locked(request_id, &payload_sha)
    }

    fn replay_native_remix_locked(
        &self,
        request_id: &str,
        payload_sha: &str,
    ) -> Result<Option<(String, String, PublishResult)>, AppError> {
        let Some((receipt, result)) = self.find_native_publication_locked(request_id)? else {
            return Ok(None);
        };
        let creation = self.remix_creation_locked(request_id, payload_sha)?;
        if receipt.payload_sha256 != payload_sha
            || receipt.source_id.as_deref() != Some(&creation.source_id)
            || receipt.publisher.is_some()
            || !result
                .record
                .authoring_source_ids
                .contains(&creation.source_id)
        {
            return Err(AppError::Invalid("remix requestId payload conflict".into()));
        }
        Ok(Some((
            creation.source_id,
            creation.source_directory,
            result,
        )))
    }

    /// Commit a new App and its native source affinity in one record write.
    /// Creation must have issued the source proof for this exact request.
    pub fn publish_native_remix_once<F>(
        &self,
        request_id: &str,
        payload: &serde_json::Value,
        source_id: &str,
        source_directory: &str,
        source_path: &Path,
        expected_digest: &str,
        market_origin: Option<MarketOrigin>,
        validate: F,
    ) -> Result<PublishResult, AppError>
    where
        F: Fn(&str) -> Result<rooted_fs::RootIdentity, AppError>,
    {
        if !valid_uuid(request_id) || !valid_uuid(source_id) {
            return Err(AppError::Invalid("invalid remix request identity".into()));
        }
        let payload_sha = hash(&serde_json::to_vec(payload)?);
        let _lock = self.lock()?;
        if let Some((id, source, result)) =
            self.replay_native_remix_locked(request_id, &payload_sha)?
        {
            if id != source_id || source != source_directory {
                return Err(AppError::Invalid("remix source identity mismatch".into()));
            }
            return Ok(result);
        }
        let creation = self.remix_creation_locked(request_id, &payload_sha)?;
        if creation.source_id != source_id || creation.source_directory != source_directory {
            return Err(AppError::Invalid("remix source identity mismatch".into()));
        }
        let root = self.root.join("pending-authoring");
        let pin = rooted_fs::root_identity(&root)
            .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
        let proof = self
            .read_native_authoring_source(&root, &pin, source_directory)?
            .ok_or_else(|| AppError::NotFound("authoring source".into()))?;
        let source_path = source_path
            .to_str()
            .ok_or_else(|| AppError::Invalid("invalid sourcePath".into()))?;
        if proof.source_id != source_id
            || proof.source_identity != creation.source_identity
            || validate(source_directory)? != creation.source_identity
            || native_source_identity(source_path)? != creation.source_identity
            || self.find_native_source_owner_locked(source_id)?.is_some()
        {
            return Err(AppError::Invalid(
                "remix source changed or is already owned".into(),
            ));
        }
        let digest = self.fingerprint_publication(Path::new(source_path))?;
        if digest != expected_digest {
            return Err(AppError::Invalid("remix source content changed".into()));
        }
        self.publish_locked(
            PublishRequest {
                app_id: None,
                expected_revision: 0,
                source_path: source_path.into(),
                authoring: None,
                market_origin,
            },
            None,
            Some((request_id, &payload_sha, &digest)),
            Some((
                source_directory,
                source_id,
                creation.source_identity,
                &validate,
            )),
        )
    }

    fn find_native_publication_locked(
        &self,
        request_id: &str,
    ) -> Result<Option<(AuthoringPublicationReceipt, PublishResult)>, AppError> {
        let mut found = None;
        for entry in fs::read_dir(self.root.join("apps"))? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                return Err(AppError::Invalid("symlink app directory".into()));
            }
            if !entry.file_type()?.is_dir() || !entry.path().join("record.json").exists() {
                continue;
            }
            let record = self.get_any(&entry.file_name().to_string_lossy())?;
            for receipt in &record.authoring_publications {
                if receipt.request_id != request_id {
                    continue;
                }
                if found.is_some() || record.removed {
                    return Err(AppError::Invalid("publication requestId conflict".into()));
                }
                let saved: AppRecord = serde_json::from_value(receipt.record.clone())?;
                if saved.app_id != record.app_id
                    || saved.latest_revision != receipt.revision
                    || !saved.authoring_publications.is_empty()
                {
                    return Err(AppError::Invalid("invalid publication receipt".into()));
                }
                let release = self.verify_release(&record.app_id, receipt.revision)?;
                found = Some((
                    receipt.clone(),
                    PublishResult {
                        record: saved,
                        release,
                    },
                ));
            }
        }
        Ok(found)
    }

    fn publish_inner(
        &self,
        request: PublishRequest,
        mobile_owner: Option<(
            &str,
            &str,
            Option<&ProjectRef>,
            Option<&str>,
            bool,
            &dyn Fn(&AuthoringRef) -> Result<rooted_fs::RootIdentity, AppError>,
        )>,
    ) -> Result<PublishResult, AppError> {
        let _lock = self.lock()?;
        self.publish_locked(request, mobile_owner, None, None)
    }

    fn publish_locked(
        &self,
        request: PublishRequest,
        mobile_owner: Option<(
            &str,
            &str,
            Option<&ProjectRef>,
            Option<&str>,
            bool,
            &dyn Fn(&AuthoringRef) -> Result<rooted_fs::RootIdentity, AppError>,
        )>,
        publication: Option<(&str, &str, &str)>,
        native_source: Option<(
            &str,
            &str,
            rooted_fs::RootIdentity,
            &dyn Fn(&str) -> Result<rooted_fs::RootIdentity, AppError>,
        )>,
    ) -> Result<PublishResult, AppError> {
        if request.authoring.is_some() {
            return Err(AppError::Invalid(
                "authoring requires a native owner receipt".into(),
            ));
        }
        if native_source.is_some() && mobile_owner.is_some() {
            return Err(AppError::Invalid("invalid new source publication".into()));
        }
        let mut preferred_native = None;
        let pending_authoring = if let Some((
            source,
            session,
            expected_project,
            expected_source_id,
            set_current,
            validate,
        )) = mobile_owner
        {
            let root = self.root.join("pending-authoring");
            let pin = rooted_fs::root_identity(&root)
                .map_err(|e| AppError::Invalid(format!("invalid authoring receipt root: {e}")))?;
            let source_proof = self
                .read_native_authoring_source(&root, &pin, source)?
                .ok_or_else(|| AppError::NotFound("authoring source".into()))?;
            if expected_source_id.is_some_and(|id| id != source_proof.source_id) {
                return Err(AppError::Invalid("authoring sourceId mismatch".into()));
            }
            let source_owner = self.find_native_source_owner_locked(&source_proof.source_id)?;
            if source_owner.as_deref() != request.app_id.as_deref() {
                return Err(AppError::Invalid(
                    "authoring source is not owned by this application".into(),
                ));
            }
            let mut owner = None;
            for entry in fs::read_dir(&root)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| AppError::Invalid("invalid authoring receipt name".into()))?;
                if !name.starts_with("owner-") || !name.ends_with(".json") {
                    continue;
                }
                let snapshot = rooted_fs::read_file_single_link_checked(&root, Path::new(&name))
                    .map_err(|e| AppError::Invalid(format!("invalid authoring receipt: {e}")))?;
                if !snapshot.matches_root(Some(&pin)) {
                    return Err(AppError::Invalid("authoring receipt root changed".into()));
                }
                let content = snapshot
                    .content
                    .ok_or_else(|| AppError::Invalid("authoring receipt disappeared".into()))?;
                let receipt: NativeAuthoringReceipt = serde_json::from_str(&content)?;
                if receipt.authoring.source_directory != source
                    || receipt.authoring.authoring_session_id.as_deref() != Some(session)
                {
                    continue;
                }
                validate_authoring_binding(&receipt.authoring)?;
                let project = receipt.authoring.project_ref.as_ref().unwrap();
                if expected_project.is_some_and(|expected| expected != project) {
                    continue;
                }
                if name != Self::native_authoring_receipt_name(source, project, session)
                    || receipt.source_id != source_proof.source_id
                    || receipt.source_identity != source_proof.source_identity
                    || receipt
                        .authoring
                        .app_id
                        .as_deref()
                        .is_some_and(|id| Some(id) != request.app_id.as_deref())
                    || owner.is_some()
                {
                    return Err(AppError::Invalid(
                        "ambiguous or inconsistent authoring owner".into(),
                    ));
                }
                owner = Some(receipt);
            }
            let receipt = owner
                .ok_or_else(|| AppError::NotFound("authoring owner for engine session".into()))?;
            if validate(&receipt.authoring)? != receipt.source_identity
                || rooted_fs::root_identity(&root).ok().as_ref() != Some(&pin)
            {
                return Err(AppError::Invalid(
                    "authoring source changed before publication".into(),
                ));
            }
            if request.app_id.is_none() {
                let name = format!("{}.json", hash(source.as_bytes()));
                let pending = rooted_fs::read_file_single_link_checked(&root, Path::new(&name))
                    .map_err(|e| AppError::Invalid(format!("invalid preferred authoring: {e}")))?;
                if !pending.matches_root(Some(&pin)) {
                    return Err(AppError::Invalid("authoring receipt root changed".into()));
                }
                if let Some(content) = pending.content {
                    let preferred: AuthoringRef = serde_json::from_str(&content)?;
                    validate_authoring_binding(&preferred)?;
                    if preferred.source_directory != source || preferred.app_id.is_some() {
                        return Err(AppError::Invalid(
                            "invalid preferred authoring owner".into(),
                        ));
                    }
                    let preferred_receipt = self
                        .read_native_authoring_receipt(
                            &root,
                            &pin,
                            source,
                            preferred.project_ref.as_ref().unwrap(),
                            preferred.authoring_session_id.as_deref().unwrap(),
                        )?
                        .ok_or_else(|| {
                            AppError::Invalid("preferred authoring receipt missing".into())
                        })?;
                    if preferred_receipt.authoring != preferred
                        || preferred_receipt.source_id != source_proof.source_id
                        || preferred_receipt.source_identity != source_proof.source_identity
                        || validate(&preferred)? != source_proof.source_identity
                    {
                        return Err(AppError::Invalid(
                            "preferred authoring proof mismatch".into(),
                        ));
                    }
                    preferred_native = Some(preferred);
                }
            }
            Some((
                self.pending_authoring_path(source),
                receipt.authoring,
                receipt.source_identity,
                source_proof.source_id,
                set_current,
            ))
        } else {
            None
        };
        let mut record = if let Some(app_id) = &request.app_id {
            let record = self.get(app_id)?;
            if mobile_owner.is_none() && !record.authoring_source_ids.is_empty() {
                return Err(AppError::Invalid(
                    "authoring publication requires an owner receipt".into(),
                ));
            }
            if record.latest_revision != request.expected_revision {
                return Err(AppError::Conflict {
                    expected: request.expected_revision,
                    actual: record.latest_revision,
                });
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
                authoring_source_ids: Vec::new(),
                authoring_publications: Vec::new(),
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
        if publication.is_some_and(|(_, _, expected)| release.digest != expected) {
            let _ = fs::remove_dir_all(&stage);
            return Err(AppError::Invalid(
                "staged publication digest changed".into(),
            ));
        }
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
        if let Some(authoring) = pending_authoring
            .as_ref()
            .map(|(_, authoring, _, _, _)| authoring.clone())
        {
            validate_authoring_owner(&authoring, &record.app_id)?;
            if mobile_owner.is_some() {
                if revision == 1 {
                    record.authoring = Some(authoring_with_owner(
                        preferred_native.unwrap_or(authoring),
                        &record.app_id,
                    ));
                }
            } else if record.authoring.is_none() || record.latest_revision == 1 {
                record.authoring = Some(authoring_with_owner(authoring, &record.app_id));
            }
        }
        if let Some((_, _, _, source_id, _)) = &pending_authoring {
            if !record.authoring_source_ids.contains(source_id) {
                record.authoring_source_ids.push(source_id.clone());
            }
        }
        if let Some((_, source_id, _, _)) = native_source {
            record.authoring_source_ids.push(source_id.to_owned());
        }
        if record.market_origin.is_none() {
            record.market_origin = request.market_origin;
        }
        if revision == 1 {
            record.data_schema_version = manifest.schema_version();
        }
        if let (Some((_, authoring, identity, _, _)), Some((_, _, _, _, _, validate))) =
            (&pending_authoring, mobile_owner)
        {
            if validate(authoring)? != *identity {
                return Err(AppError::Invalid(
                    "authoring source or engine changed before commit".into(),
                ));
            }
        }
        if let Some((source, _, identity, validate)) = native_source {
            if validate(source)? != identity {
                return Err(AppError::Invalid("new source changed before commit".into()));
            }
        }
        let response_record = if let Some((request_id, payload_sha, _)) = publication {
            let mut response = record.clone();
            response.authoring_publications.clear();
            record
                .authoring_publications
                .push(AuthoringPublicationReceipt {
                    request_id: request_id.into(),
                    payload_sha256: payload_sha.into(),
                    revision,
                    publisher: pending_authoring
                        .as_ref()
                        .map(|(_, authoring, _, _, _)| authoring.clone()),
                    source_id: pending_authoring
                        .as_ref()
                        .map(|(_, _, _, source_id, _)| source_id.clone())
                        .or_else(|| native_source.map(|(_, source_id, _, _)| source_id.to_owned())),
                    record: serde_json::to_value(&response)?,
                });
            Some(response)
        } else {
            None
        };
        self.save(&record)?;
        if let Some((path, _, _, _, _)) = pending_authoring {
            // The record is already committed; a stale preferred pointer is
            // not proof of an owner and is ignored for subsequent updates.
            if fs::remove_file(&path).is_ok() {
                if let Some(parent) = path.parent() {
                    if let Ok(directory) = File::open(parent) {
                        let _ = directory.sync_all();
                    }
                }
            }
        }
        Ok(PublishResult {
            record: response_record.unwrap_or(record),
            release,
        })
    }
    /// Verify the release file list, contents, executable flags, and aggregate digest against disk.
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
    /// Verify and activate a release only when its data schema matches retained app data.
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
    /// Record execution consent for a known, verified release digest.
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
    /// Remove all recorded execution grants for an app.
    pub fn revoke_execution(&self, app_id: &str) -> Result<AppRecord, AppError> {
        let _lock = self.lock()?;
        let mut record = self.get(app_id)?;
        record.execution_grants.clear();
        record.updated_at_ms = now();
        self.save(&record)?;
        Ok(record)
    }
    /// Verify the release and reject execution when its manifest requires unrecorded consent.
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
    /// Prepare a fresh run, reusing a matching previously prepared environment when available.
    pub fn prepare_run(&self, app_id: &str, revision: u64) -> Result<RunPreparation, AppError> {
        self.prepare_run_with_options(app_id, revision, false)
    }
    /// Verify consent and data schema, then stage a run; `rebuild` bypasses the cached environment.
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
    /// Read validated run records for an app, sorted by their start time.
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
    /// Persist a run transition while enforcing immutable identity and preventing resurrection of terminal runs.
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
    /// Append run output, retaining only the latest bounded log bytes.
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
    /// Read a run log with lossy UTF-8 decoding, or an empty string if no log exists.
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
    /// Mark an app removed and clear its grants, rejecting removal while any run is live.
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
    /// Copy a verified public file allowlist to a new absolute directory without exporting retained app data.
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
    /// Copy an immutable release into a new native authoring directory.
    ///
    /// Call this from the creation callback of `create_native_authoring_source`
    /// so release validation, copying, and issuance of the source proof occur
    /// under the same library lock. It does not change the current authoring
    /// pointer. A failed copy may leave an unacknowledged partial directory.
    pub fn copy_native_authoring_source(
        &self,
        app_id: &str,
        revision: u64,
        destination: &Path,
    ) -> Result<(), AppError> {
        let record = self.get(app_id)?;
        if record.authoring.is_none()
            && record
                .market_origin
                .as_ref()
                .is_some_and(|origin| origin.remixed_from.is_none())
        {
            return Err(AppError::Invalid(
                "market installations require an authorized remix".into(),
            ));
        }
        let release = self.verify_release(app_id, revision)?;
        self.copy_native_release_source(app_id, revision, destination, &release, None)
    }

    fn copy_native_release_source(
        &self,
        app_id: &str,
        revision: u64,
        destination: &Path,
        release: &AppRelease,
        replacement_manifest: Option<&Manifest>,
    ) -> Result<(), AppError> {
        if !destination.is_absolute() || destination.starts_with(&self.root) {
            return Err(AppError::Invalid(
                "invalid native source destination".into(),
            ));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| AppError::Invalid("source parent missing".into()))?;
        let leaf = destination
            .file_name()
            .ok_or_else(|| AppError::Invalid("source name missing".into()))?;
        let parent_pin = rooted_fs::root_identity(parent)
            .map_err(|e| AppError::Invalid(format!("invalid source parent: {e}")))?;
        let opened =
            rooted_fs::create_directory_new_pinned(parent, Path::new(leaf), 0o700, &parent_pin)
                .map_err(|e| AppError::Invalid(format!("cannot create authoring source: {e}")))?;
        let source_pin = rooted_fs::opened_file_identity(&opened)
            .map_err(|e| AppError::Invalid(format!("invalid created source: {e}")))?;
        let immutable_source = self.source_path(app_id, revision)?;
        for file in &release.files {
            let original = bytes(&checked_file(&immutable_source, &file.path)?)?;
            if hash(&original) != file.sha256 || original.len() as u64 != file.bytes {
                return Err(AppError::Invalid(
                    "immutable release changed during source copy".into(),
                ));
            }
            let content = if file.path == MANIFEST_FILE {
                replacement_manifest
                    .map(serde_json::to_vec_pretty)
                    .transpose()?
                    .unwrap_or(original)
            } else {
                original
            };
            let options = rooted_fs::AtomicWriteOptions {
                overwrite: false,
                create_parents: true,
                dir_mode: 0o700,
                file_mode: if file.executable { 0o700 } else { 0o600 },
            };
            rooted_fs::atomic_write_pinned(
                destination,
                Path::new(&file.path),
                &content,
                options,
                &source_pin,
            )
            .map_err(|e| AppError::Invalid(format!("cannot copy authoring source: {e}")))?;
        }
        if rooted_fs::root_identity(parent).ok().as_ref() != Some(&parent_pin)
            || rooted_fs::root_identity(destination).ok().as_ref() != Some(&source_pin)
        {
            return Err(AppError::Invalid(
                "authoring source changed during copy".into(),
            ));
        }
        Ok(())
    }

    /// Prepare a fresh source from a remix-authorized immutable release. This
    /// does not change the original application's current authoring pointer.
    /// Call under `create_native_authoring_source`'s creation callback.
    pub fn copy_native_remix_source(
        &self,
        app_id: &str,
        revision: u64,
        destination: &Path,
        name: Option<&str>,
    ) -> Result<(), AppError> {
        let (release, manifest) = self.native_remix_release(app_id, revision, name)?;
        self.copy_native_release_source(app_id, revision, destination, &release, Some(&manifest))
    }

    fn native_remix_release(
        &self,
        app_id: &str,
        revision: u64,
        name: Option<&str>,
    ) -> Result<(AppRelease, Manifest), AppError> {
        let release = self.verify_release(app_id, revision)?;
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
        let mut manifest = release.manifest.clone();
        if let Some(name) = name {
            manifest.name = name.into();
            manifest.validate()?;
        }
        Ok((release, manifest))
    }

    /// Expected normalized source digest and immutable origin for a remix.
    pub fn native_remix_source_metadata(
        &self,
        app_id: &str,
        revision: u64,
        name: Option<&str>,
    ) -> Result<(String, Option<MarketOrigin>), AppError> {
        let (release, manifest) = self.native_remix_release(app_id, revision, name)?;
        let digest = fingerprint_manifest_inventory(&manifest, &release.files)?;
        let origin = release.market_origin.map(|mut origin| {
            origin.remixed_from = Some(origin.app_id.clone());
            origin
        });
        Ok((digest, origin))
    }
    /// Decode a typed library request from JSON and dispatch it through the host library interface.
    pub fn request_json(&self, request: &str) -> Result<serde_json::Value, AppError> {
        let request: LibraryRequest = serde_json::from_str(request)?;
        self.dispatch(request)
    }

    // Generic JSON cannot claim an authoring owner. Check the physical source
    // and its ancestors under the same library lock used to issue source proof,
    // then publish without releasing that lock.
    fn publish_unbound(&self, request: PublishRequest) -> Result<PublishResult, AppError> {
        if request.authoring.is_some() {
            return Err(AppError::Invalid(
                "authoring requires a native owner receipt".into(),
            ));
        }
        let _lock = self.lock()?;
        self.reject_registered_source_ancestors_locked(Path::new(&request.source_path))?;
        self.publish_locked(request, None, None, None)
    }

    // The caller holds the library lock, so source proof creation cannot race
    // this check and the subsequent publication.
    fn reject_registered_source_ancestors_locked(&self, source: &Path) -> Result<(), AppError> {
        if !source.is_absolute() {
            return Err(AppError::Invalid("sourcePath must be absolute".into()));
        }
        let source = fs::canonicalize(source)?;
        let root = self.root.join("pending-authoring");
        let pin = rooted_fs::root_identity(&root).map_err(|error| {
            AppError::Invalid(format!("invalid authoring receipt root: {error}"))
        })?;
        let ancestors = || {
            source
                .ancestors()
                .filter(|path| cfg!(windows) || path.parent().is_some())
        };
        for path in ancestors() {
            let path = path
                .to_str()
                .ok_or_else(|| AppError::Invalid("source path must be UTF-8".into()))?;
            if self
                .read_native_authoring_source(&root, &pin, path)?
                .is_some()
            {
                return Err(AppError::Invalid(
                    "registered authoring source requires an owner receipt".into(),
                ));
            }
        }
        #[cfg(windows)]
        {
            // The opened directory identity closes aliases that Windows may
            // consider equal even when their Unicode spellings differ.
            let identities = ancestors()
                .map(|path| {
                    rooted_fs::root_identity(path).map_err(|error| {
                        AppError::Invalid(format!("cannot identify source ancestor: {error}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if self.native_source_identity_registered(&root, &pin, &identities)? {
                return Err(AppError::Invalid(
                    "registered authoring source requires an owner receipt".into(),
                ));
            }
        }
        if rooted_fs::root_identity(&root).ok().as_ref() != Some(&pin) {
            return Err(AppError::Invalid("authoring receipt root changed".into()));
        }
        Ok(())
    }

    /// Execute a typed host library request and encode its result as JSON.
    pub fn dispatch(&self, request: LibraryRequest) -> Result<serde_json::Value, AppError> {
        use LibraryRequest::*;
        Ok(match request {
            List {} => serde_json::to_value(self.list()?)?,
            Get { app_id } => serde_json::to_value(self.get(&app_id)?)?,
            Publish { request } => serde_json::to_value(self.publish_unbound(request)?)?,
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
                market_origin,
            } => serde_json::to_value(self.publish_unbound(PublishRequest {
                app_id: None,
                expected_revision: 0,
                source_path,
                authoring: None,
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
        })
    }
}
