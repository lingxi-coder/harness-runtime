//! `~/.lingxi/ide/<port>.lock` writer and Drop-guard.
//!
//! Wire format matches claude-code's `LockfileJsonContent` in
//! `src/utils/ide.ts`. The file is JSON with keys:
//! `pid`, `workspaceFolders`, `ideName`, `transport`, `runningInWindows`,
//! `authToken`. The port is encoded in the filename (`<port>.lock`),
//! NOT in the JSON body.

use rand::rngs::OsRng;
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

/// The literal `ideName` value we publish in the `~/.lingxi/ide/` lockfile,
/// scanned by the real IDE peer (claude-code, VS Code Claude, …).
pub const IDE_NAME: &str = "LingXi";
/// The literal `ideName` value we publish in the dedicated `~/.lingxi/bridge/`
/// discovery lockfile. Deliberately DISTINCT from [`IDE_NAME`] so the real IDE
/// peer's `~/.lingxi/ide/` scan never picks up the bridge's lockfile.
pub const BRIDGE_IDE_NAME: &str = "LingXi-Bridge";
/// The literal `transport` value we publish — always `"ws"` for this bridge.
pub const TRANSPORT: &str = "ws";

/// JSON body of a lockfile (matches claude-code's `LockfileJsonContent`).
///
/// Field names are camelCase to match the wire format byte-for-byte.
#[derive(Clone, Serialize, Deserialize)]
pub struct LockfileBody {
    /// Process ID of the bridge that wrote the file.
    pub pid: u32,
    /// Absolute paths of workspace folders this session owns.
    #[serde(rename = "workspaceFolders")]
    pub workspace_folders: Vec<PathBuf>,
    /// Human-readable IDE name displayed in the picker.
    #[serde(rename = "ideName")]
    pub ide_name: String,
    /// Transport selector — `"ws"` (this crate) or `"sse"` (older IDEs).
    pub transport: String,
    /// True when this bridge is hosted on Windows.
    #[serde(rename = "runningInWindows")]
    pub running_in_windows: bool,
    /// 32-char lowercase hex token a client MUST present in the
    /// `X-LingXi-Ide-Authorization` header.
    #[serde(rename = "authToken")]
    pub auth_token: String,
}

// Lockfiles are routinely included in diagnostic values. Never let the local
// bearer escape through a derived `Debug` implementation (the wire token is
// intentionally kept out of status, telemetry, and logs).
impl fmt::Debug for LockfileBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockfileBody")
            .field("pid", &self.pid)
            .field("workspace_folders", &self.workspace_folders)
            .field("ide_name", &self.ide_name)
            .field("transport", &self.transport)
            .field("running_in_windows", &self.running_in_windows)
            .field("auth_token", &"<redacted>")
            .finish()
    }
}

/// Self-describing lockfile: knows its directory, its port, and its JSON body.
#[derive(Clone)]
pub struct IdeLockfile {
    ide_dir: PathBuf,
    port: u16,
    body: LockfileBody,
}

impl fmt::Debug for IdeLockfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdeLockfile")
            .field("ide_dir", &self.ide_dir)
            .field("port", &self.port)
            .field("body", &self.body)
            .finish()
    }
}

impl IdeLockfile {
    fn ensure_private_directory(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.ide_dir)?;
        let metadata = std::fs::symlink_metadata(&self.ide_dir)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "lockfile directory must be a real directory, not a symlink",
            ));
        }
        #[cfg(unix)]
        std::fs::set_permissions(&self.ide_dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    /// Generate a 32-char lowercase hex auth token from `OsRng`.
    #[must_use]
    pub fn generate_auth_token() -> String {
        let mut bytes = [0u8; 16];
        // `OsRng::try_fill_bytes` returns Result in rand 0.9; unwrap is fine for
        // an OS-level source on healthy systems and the alternative is to abort.
        OsRng
            .try_fill_bytes(&mut bytes)
            .expect("OsRng must fill bytes");
        let mut out = String::with_capacity(32);
        for b in bytes {
            use std::fmt::Write;
            write!(&mut out, "{b:02x}").expect("write to String is infallible");
        }
        out
    }

    /// Build a lockfile body with a fresh auth token and the IDE-peer
    /// [`IDE_NAME`].
    #[must_use]
    pub fn new_body(workspace_folders: Vec<PathBuf>) -> LockfileBody {
        Self::new_body_with_ide_name(IDE_NAME, workspace_folders)
    }

    /// Build a lockfile body with a fresh auth token and an explicit `ide_name`
    /// (e.g. [`BRIDGE_IDE_NAME`] for the dedicated bridge discovery file).
    #[must_use]
    pub fn new_body_with_ide_name(ide_name: &str, workspace_folders: Vec<PathBuf>) -> LockfileBody {
        LockfileBody {
            pid: std::process::id(),
            workspace_folders,
            ide_name: ide_name.to_string(),
            transport: TRANSPORT.to_string(),
            running_in_windows: cfg!(target_os = "windows"),
            auth_token: Self::generate_auth_token(),
        }
    }

    /// Construct an `IdeLockfile` rooted at the user's `~/.lingxi/ide` dir.
    /// Creates the dir on first call (mode 0o755 on Unix).
    ///
    /// # Errors
    /// Returns I/O errors if the home directory cannot be resolved or the
    /// `~/.lingxi/ide` directory cannot be created.
    pub fn for_user(port: u16, workspace_folders: Vec<PathBuf>) -> std::io::Result<Self> {
        // Honor `$LINGXI_CONFIG_DIR` (set+non-empty) else `~/.lingxi`, like the
        // rest of the config-home tree, so the IDE discovery lockfile lands where
        // the engine/peers look for it.
        let config_home = std::env::var_os(branding::CONFIG_DIR_ENV)
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(branding::DOT_DIR)))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory")
            })?;
        let ide_dir = config_home.join("ide");
        std::fs::create_dir_all(&ide_dir)?;
        Ok(Self::new_for_ide_dir(ide_dir, port, workspace_folders))
    }

    /// Construct an `IdeLockfile` rooted at an arbitrary `ide_dir`. Used by tests
    /// with `tempfile::TempDir` instead of `$HOME`.
    #[must_use]
    pub fn new_for_ide_dir(ide_dir: PathBuf, port: u16, workspace_folders: Vec<PathBuf>) -> Self {
        Self {
            ide_dir,
            port,
            body: Self::new_body(workspace_folders),
        }
    }

    /// Construct a DEDICATED bridge discovery lockfile rooted at the user's
    /// `~/.lingxi/bridge` dir, carrying the distinct [`BRIDGE_IDE_NAME`].
    /// Creates the dir on first call (mode 0o755 on Unix).
    ///
    /// This is the F2-04 discovery file the Electron app reads: it lives in its
    /// own directory with its own `ideName` so it never collides with the real
    /// IDE peer scanning `~/.lingxi/ide/`.
    ///
    /// # Errors
    /// Returns I/O errors if the home directory cannot be resolved or the
    /// `~/.lingxi/bridge` directory cannot be created.
    pub fn for_bridge(port: u16, workspace_folders: Vec<PathBuf>) -> std::io::Result<Self> {
        // Honor `$LINGXI_CONFIG_DIR` (set+non-empty) else `~/.lingxi`, matching
        // the config-home tree (the Electron app reads this discovery file).
        let config_home = std::env::var_os(branding::CONFIG_DIR_ENV)
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(branding::DOT_DIR)))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory")
            })?;
        let bridge_dir = config_home.join("bridge");
        std::fs::create_dir_all(&bridge_dir)?;
        Ok(Self::new_for_bridge_dir(
            bridge_dir,
            port,
            workspace_folders,
        ))
    }

    /// Construct a bridge discovery lockfile rooted at an arbitrary
    /// `bridge_dir`, carrying the distinct [`BRIDGE_IDE_NAME`]. Used by tests
    /// with `tempfile::TempDir` instead of `$HOME`.
    #[must_use]
    pub fn new_for_bridge_dir(
        bridge_dir: PathBuf,
        port: u16,
        workspace_folders: Vec<PathBuf>,
    ) -> Self {
        Self {
            ide_dir: bridge_dir,
            port,
            body: Self::new_body_with_ide_name(BRIDGE_IDE_NAME, workspace_folders),
        }
    }

    /// Full path of this lockfile.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.ide_dir.join(format!("{}.lock", self.port))
    }

    /// Borrow the JSON body (useful for tests).
    #[must_use]
    pub fn body(&self) -> &LockfileBody {
        &self.body
    }

    /// Borrow the auth token.
    #[must_use]
    pub fn auth_token(&self) -> &str {
        &self.body.auth_token
    }

    /// Port encoded in the filename.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Human-readable IDE name from the lockfile.
    #[must_use]
    pub fn ide_name(&self) -> &str {
        &self.body.ide_name
    }

    /// Transport selector (`"ws"` or `"sse"`).
    #[must_use]
    pub fn transport(&self) -> &str {
        &self.body.transport
    }

    /// Workspace folders advertised by the IDE peer.
    #[must_use]
    pub fn workspace_folders(&self) -> &[PathBuf] {
        &self.body.workspace_folders
    }

    /// Whether the peer expects Windows path semantics.
    #[must_use]
    pub fn running_in_windows(&self) -> bool {
        self.body.running_in_windows
    }

    /// Endpoint URL derived from the lockfile's loopback port and transport.
    #[must_use]
    pub fn endpoint_url(&self) -> String {
        if self.transport() == "sse" {
            format!("http://127.0.0.1:{}/sse", self.port)
        } else {
            // Claude's IDE detector targets the loopback root for WebSocket
            // lockfiles (the bridge accepts both root and `/mcp` paths).
            format!("ws://127.0.0.1:{}", self.port)
        }
    }

    /// Exclusively write the JSON body as a private regular file.
    ///
    /// # Errors
    /// Returns I/O errors if the directory is unsafe, serialization fails, or a
    /// path (including a stale file or symlink) already occupies this port.
    pub fn write(&self) -> std::io::Result<()> {
        let serialized = serde_json::to_vec_pretty(&self.body).map_err(std::io::Error::other)?;
        self.ensure_private_directory()?;

        let path = self.path();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&path)?;
        if let Err(error) = file.write_all(&serialized) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        let metadata = file.metadata()?;
        if !metadata.is_file() {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "lockfile path is not a regular file",
            ));
        }
        Ok(())
    }

    /// Parse a lockfile from disk. Returns the body plus the port encoded in
    /// the filename.
    ///
    /// # Errors
    /// Returns `InvalidData` if the file is not well-formed JSON matching
    /// [`LockfileBody`], `InvalidInput` if the filename isn't `<port>.lock`,
    /// or the underlying I/O error from reading the file.
    pub fn read(path: &Path) -> std::io::Result<(LockfileBody, u16)> {
        let raw = std::fs::read_to_string(path)?;
        let body: LockfileBody = serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let port = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".lock"))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "lockfile name not <port>.lock",
                )
            })?;
        Ok((body, port))
    }

    /// Parse and validate a lockfile before using it as a local endpoint.
    ///
    /// Discovery is an authority boundary: a symlink, group/world-readable
    /// file, or lockfile owned by another user must never be allowed to supply
    /// an auth token to a client connection. The file is opened with
    /// `O_NOFOLLOW` on Unix after the metadata checks, closing the usual
    /// check-then-open symlink race. `read` remains the intentionally lenient
    /// parser used by compatibility callers and tests; all endpoint discovery
    /// goes through this method.
    pub fn read_secure(path: &Path) -> std::io::Result<(LockfileBody, u16)> {
        Self::read_secure_with(path, is_process_alive)
    }

    /// Testable form of [`Self::read_secure`] with an injected liveness probe.
    pub fn read_secure_with<F>(
        path: &Path,
        process_alive: F,
    ) -> std::io::Result<(LockfileBody, u16)>
    where
        F: Fn(u32) -> bool,
    {
        let port = Self::parse_port_from_path(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(invalid_lockfile("lockfile is not a regular file"));
        }

        #[cfg(unix)]
        {
            let expected_uid = nix::unistd::geteuid().as_raw();
            if metadata.uid() != expected_uid {
                return Err(invalid_lockfile(
                    "lockfile owner does not match current user",
                ));
            }
            // The writer creates 0600 files. Requiring no group/other bits is
            // the important invariant; retaining owner read/write allows a
            // manually-created 0400 lockfile to be diagnosed as a valid,
            // read-only endpoint rather than silently following it.
            if metadata.mode() & 0o077 != 0 {
                return Err(invalid_lockfile("lockfile permissions are too broad"));
            }
        }

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(nix::libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        let opened_metadata = file.metadata()?;
        if !opened_metadata.is_file() {
            return Err(invalid_lockfile("lockfile is not a regular file"));
        }
        #[cfg(unix)]
        {
            // Re-check the opened handle and make sure it is the same inode
            // examined above. This closes a rename/hard-link replacement
            // between `symlink_metadata` and `open`, in addition to
            // `O_NOFOLLOW`'s symlink protection.
            if opened_metadata.uid() != nix::unistd::geteuid().as_raw()
                || opened_metadata.mode() & 0o077 != 0
                || opened_metadata.dev() != metadata.dev()
                || opened_metadata.ino() != metadata.ino()
            {
                return Err(invalid_lockfile("lockfile changed during discovery"));
            }
        }
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)?;
        let body: LockfileBody = serde_json::from_slice(&raw)
            .map_err(|error| invalid_lockfile(format!("malformed JSON: {error}")))?;
        validate_body(&body, port, &process_alive)?;
        Ok((body, port))
    }

    fn parse_port_from_path(path: &Path) -> std::io::Result<u16> {
        path.file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".lock"))
            // `u16::from_str` accepts a leading `+`; the on-disk contract is
            // the literal `<port>.lock` decimal form, so reject signs and
            // every other non-digit spelling before parsing.
            .filter(|s| !s.is_empty() && s.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid_lockfile("lockfile name not <port>.lock"))
    }
}

fn invalid_lockfile(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn validate_body<F>(body: &LockfileBody, _port: u16, process_alive: &F) -> std::io::Result<()>
where
    F: Fn(u32) -> bool,
{
    if body.pid == 0 || !process_alive(body.pid) {
        return Err(invalid_lockfile("lockfile process is not alive"));
    }
    if body.ide_name.trim().is_empty() {
        return Err(invalid_lockfile("lockfile ideName is empty"));
    }
    if body
        .ide_name
        .chars()
        .any(|character| character.is_control())
    {
        return Err(invalid_lockfile(
            "lockfile ideName contains control characters",
        ));
    }
    if !matches!(body.transport.as_str(), "ws" | "sse") {
        return Err(invalid_lockfile("lockfile transport is not ws or sse"));
    }
    if body.auth_token.is_empty()
        || !body.auth_token.is_ascii()
        || body
            .auth_token
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(invalid_lockfile("lockfile authToken is invalid"));
    }
    if body
        .workspace_folders
        .iter()
        .any(|folder| !folder.is_absolute())
    {
        return Err(invalid_lockfile(
            "lockfile workspace folder is not absolute",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM))
}

#[cfg(not(unix))]
fn is_process_alive(pid: u32) -> bool {
    // Windows does not expose a safe, dependency-free process-liveness probe
    // in this crate. The endpoint still gets strict file/JSON validation; a
    // subsequent local connection failure rejects an expired endpoint.
    pid != 0
}

/// Discover every valid local IDE lockfile in deterministic port order.
///
/// Invalid candidates are ignored individually so one stale or attacker-owned
/// file cannot hide a healthy peer. The caller can use the resulting count to
/// implement the `--ide` rule: auto-connect only when exactly one endpoint is
/// valid.
#[must_use]
pub fn discover_all(ide_dir: &Path) -> Vec<IdeLockfile> {
    discover_all_with(ide_dir, is_process_alive)
}

/// Deterministic discovery seam used by tests and embedders that already own a
/// process-liveness oracle. Production callers should use [`discover_all`].
#[must_use]
pub fn discover_all_with<F>(ide_dir: &Path, process_alive: F) -> Vec<IdeLockfile>
where
    F: Fn(u32) -> bool,
{
    let Ok(dir_metadata) = std::fs::symlink_metadata(ide_dir) else {
        return Vec::new();
    };
    if dir_metadata.file_type().is_symlink() || !dir_metadata.is_dir() {
        return Vec::new();
    }
    #[cfg(unix)]
    {
        if dir_metadata.uid() != nix::unistd::geteuid().as_raw() || dir_metadata.mode() & 0o077 != 0
        {
            return Vec::new();
        }
    }

    let Ok(entries) = std::fs::read_dir(ide_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !name.ends_with(".lock") {
            continue;
        }
        let Ok((body, port)) = IdeLockfile::read_secure_with(&path, &process_alive) else {
            continue;
        };
        out.push(IdeLockfile {
            ide_dir: ide_dir.to_path_buf(),
            port,
            body,
        });
    }
    out.sort_by(|left, right| {
        left.port
            .cmp(&right.port)
            .then_with(|| left.body.ide_name.cmp(&right.body.ide_name))
    });
    out
}

/// Discover the most-recently-modified `<port>.lock` file in `ide_dir`.
///
/// Returns `None` if the directory does not exist, contains no `.lock` files,
/// or every candidate fails to parse. Used at bridge connect time to find a
/// running IDE peer (mirrors claude-code's `cleanupStaleIdeLockfiles` scan).
#[must_use]
pub fn discover_latest(ide_dir: &Path) -> Option<IdeLockfile> {
    let mut best: Option<(std::time::SystemTime, IdeLockfile)> = None;
    for candidate in discover_all(ide_dir) {
        let mtime = std::fs::symlink_metadata(candidate.path())
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(existing, _)| *existing < mtime) {
            best = Some((mtime, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// Drop-guard that removes a lockfile when the bridge shuts down OR panics.
///
/// `Drop::drop` MUST be infallible-on-failure: we use `std::fs::remove_file`
/// directly (sync) because the async runtime may already be torn down during
/// panic unwind. Removal errors are logged at `warn` level and swallowed —
/// failing to remove a stale lockfile is recoverable (claude-code's
/// `cleanupStaleIdeLockfiles()` will reap it next start).
pub struct LockfileGuard {
    path: Option<PathBuf>,
}

impl LockfileGuard {
    /// Take ownership of `path`. When this guard drops, the file is removed.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Surrender the guard without removing the file. Used in tests where we
    /// want to inspect the lockfile AFTER drop normally would have run.
    pub fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for LockfileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(?path, error = %e, "failed to remove lockfile in Drop");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_recovers_port_from_filename() {
        let tmp = TempDir::new().unwrap();
        let lf = IdeLockfile::new_for_ide_dir(
            tmp.path().to_path_buf(),
            54321,
            vec![PathBuf::from("/w")],
        );
        lf.write().unwrap();
        let (body, port) = IdeLockfile::read(&lf.path()).unwrap();
        assert_eq!(port, 54321);
        assert_eq!(body.transport, "ws");
        assert_eq!(body.ide_name, "LingXi");
    }

    #[test]
    fn read_rejects_filename_without_dot_lock() {
        let tmp = TempDir::new().unwrap();
        let bogus = tmp.path().join("40729.json");
        std::fs::write(
            &bogus,
            r#"{"pid":1,"workspaceFolders":[],"ideName":"x","transport":"ws","runningInWindows":false,"authToken":"a"}"#,
        )
        .unwrap();
        let err = IdeLockfile::read(&bogus).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn discover_latest_returns_none_for_empty_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(discover_latest(tmp.path()).is_none());
    }

    #[test]
    fn discover_latest_picks_most_recent() {
        let tmp = TempDir::new().unwrap();
        let older = IdeLockfile::new_for_ide_dir(
            tmp.path().to_path_buf(),
            40001,
            vec![PathBuf::from("/w/a")],
        );
        older.write().unwrap();
        // Force a later mtime on the second file.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = IdeLockfile::new_for_ide_dir(
            tmp.path().to_path_buf(),
            40002,
            vec![PathBuf::from("/w/b")],
        );
        newer.write().unwrap();
        let found = discover_latest(tmp.path()).expect("should discover");
        assert_eq!(found.port(), 40002);
    }

    #[test]
    fn discover_latest_ignores_temp_files() {
        let tmp = TempDir::new().unwrap();
        // Drop a stray `.tmp` that shouldn't be picked up.
        std::fs::write(tmp.path().join(".40729.lock.tmp"), b"{}").unwrap();
        assert!(discover_latest(tmp.path()).is_none());
    }

    #[test]
    fn disarm_skips_drop_removal() {
        let tmp = TempDir::new().unwrap();
        let lf = IdeLockfile::new_for_ide_dir(
            tmp.path().to_path_buf(),
            40729,
            vec![PathBuf::from("/w")],
        );
        lf.write().unwrap();
        let guard = LockfileGuard::new(lf.path());
        guard.disarm();
        assert!(lf.path().exists(), "disarm must preserve the file");
    }

    #[test]
    fn secure_discovery_rejects_expired_processes() {
        let tmp = TempDir::new().unwrap();
        let lf = IdeLockfile::new_for_ide_dir(tmp.path().to_path_buf(), 43126, vec![]);
        lf.write().unwrap();
        assert!(discover_all_with(tmp.path(), |_| false).is_empty());
    }

    #[test]
    fn secure_discovery_rejects_malformed_body_and_non_ascii_token() {
        let tmp = TempDir::new().unwrap();
        let malformed = IdeLockfile::new_for_ide_dir(tmp.path().to_path_buf(), 43125, vec![]);
        malformed.write().unwrap();
        std::fs::write(malformed.path(), b"not-json").unwrap();
        assert!(IdeLockfile::read_secure_with(&malformed.path(), |_| true).is_err());

        let signed_name = tmp.path().join("+43126.lock");
        std::fs::write(&signed_name, b"{}").unwrap();
        assert!(IdeLockfile::read_secure_with(&signed_name, |_| true).is_err());

        let non_ascii = IdeLockfile::new_for_ide_dir(tmp.path().to_path_buf(), 43124, vec![]);
        non_ascii.write().unwrap();
        let body = serde_json::json!({
            "pid": std::process::id(),
            "workspaceFolders": [],
            "ideName": "IDE",
            "transport": "ws",
            "runningInWindows": false,
            "authToken": "té"
        });
        std::fs::write(non_ascii.path(), serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(IdeLockfile::read_secure_with(&non_ascii.path(), |_| true).is_err());

        let control_name = IdeLockfile::new_for_ide_dir(tmp.path().to_path_buf(), 43123, vec![]);
        control_name.write().unwrap();
        let body = serde_json::json!({
            "pid": std::process::id(),
            "workspaceFolders": [],
            "ideName": "IDE\n",
            "transport": "ws",
            "runningInWindows": false,
            "authToken": "token"
        });
        std::fs::write(control_name.path(), serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(IdeLockfile::read_secure_with(&control_name.path(), |_| true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn secure_discovery_rejects_broad_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let lf = IdeLockfile::new_for_ide_dir(tmp.path().to_path_buf(), 43127, vec![]);
        lf.write().unwrap();
        std::fs::set_permissions(lf.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(discover_all_with(tmp.path(), |_| true).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn secure_discovery_rejects_symlink_candidates() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.lock");
        let link = tmp.path().join("43128.lock");
        let lf = IdeLockfile::new_for_ide_dir(tmp.path().to_path_buf(), 43129, vec![]);
        lf.write().unwrap();
        std::fs::rename(lf.path(), &target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(discover_all_with(tmp.path(), |_| true).is_empty());
    }

    #[test]
    fn secure_discovery_is_sorted_and_exposes_endpoint_without_token() {
        let tmp = TempDir::new().unwrap();
        let first = IdeLockfile::new_for_ide_dir(
            tmp.path().to_path_buf(),
            43130,
            vec![PathBuf::from("/workspace/one")],
        );
        let second = IdeLockfile::new_for_ide_dir(
            tmp.path().to_path_buf(),
            43131,
            vec![PathBuf::from("/workspace/two")],
        );
        first.write().unwrap();
        second.write().unwrap();
        let found = discover_all_with(tmp.path(), |pid| pid == std::process::id());
        assert_eq!(
            found.iter().map(IdeLockfile::port).collect::<Vec<_>>(),
            vec![43130, 43131]
        );
        assert_eq!(found[0].endpoint_url(), "ws://127.0.0.1:43130");
        let debug = format!("{:?}", found[0]);
        assert!(!debug.contains(found[0].auth_token()));
    }
}
