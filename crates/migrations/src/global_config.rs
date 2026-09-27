//! `~/.lingxi.json` `GlobalConfig` substrate — the first in the Rust port
//! (`tools/meta/src/config.rs:17` records "no substrate" prior to this).
//!
//! Ports the path/read/save mechanics of `utils/config.ts` +
//! `utils/env.ts getGlobalClaudeFile` + `utils/envUtils.ts
//! getClaudeConfigHomeDir`, operating on a raw [`serde_json::Map`] so unknown
//! keys (the real file carries dozens: `numStartups`, `oauthAccount`, …) are
//! NEVER dropped. Values equal to Claude's built-in global defaults are the
//! sole exception: the upstream writer filters those before serialization.
//! `serde_json`'s workspace `preserve_order` feature keeps key order stable
//! across round-trips.
//!
//! Documented simplifications vs TS (`config.ts:797-864`):
//! - Uses a crate-local cross-process lock dir with PID/start-time stale-owner
//!   detection instead of `proper-lockfile`; the full read-modify-write
//!   transaction is still serialized across processes so failed reads never
//!   fall back to writing defaults (the GH #3117 auth-loss guard remains N/A).
//! - TS NFC-normalizes the config-home path; macOS paths are already NFC, so
//!   this port uses the path as-is.
//! - TS `writeFileSyncAndFlush_DEPRECATED` (`file.ts:439-477`) falls back to
//!   a NON-atomic in-place write when the atomic tmp+rename path fails; this
//!   port skips the fallback and surfaces the error — migrations treat any
//!   write failure as "skip", and a torn half-write of `~/.lingxi.json` is
//!   worse than a skipped migration.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// In-memory session trust flag — `bootstrap/state.ts:153,363,1317-1322`
/// (`STATE.sessionTrustAccepted` + `setSessionTrustAccepted` /
/// `getSessionTrustAccepted`). When the trust dialog is accepted while
/// `cwd === homedir()`, claude-code stores acceptance in memory ONLY (not on
/// disk) via `setSessionTrustAccepted(true)` (`TrustDialog.tsx:174-175`), so
/// hooks/features work this run without persisting trust for `$HOME`.
static SESSION_TRUST_ACCEPTED: AtomicBool = AtomicBool::new(false);

/// `setSessionTrustAccepted` (`bootstrap/state.ts:1317-1319`): set the
/// in-memory session trust flag.
pub fn set_session_trust_accepted(accepted: bool) {
    SESSION_TRUST_ACCEPTED.store(accepted, Ordering::SeqCst);
}

/// `getSessionTrustAccepted` (`bootstrap/state.ts:1321-1322`): read the
/// in-memory session trust flag.
#[must_use]
pub fn get_session_trust_accepted() -> bool {
    SESSION_TRUST_ACCEPTED.load(Ordering::SeqCst)
}

/// `homedir()` (Node `os.homedir()`): `$HOME` resolved the SAME way the rest of
/// this crate sources it ([`lingxi_config_home`] uses `std::env::var_os("HOME")`,
/// NOT `dirs::home_dir`). Used by the `TrustDialog` accept branch to detect the
/// `cwd === homedir()` session-only case (`TrustDialog.tsx:162,174`). `None`
/// when `$HOME` is unset.
#[must_use]
pub fn trust_homedir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// A raw JSON object — the in-memory shape of `~/.lingxi.json`.
pub type JsonMap = Map<String, Value>;

const CONFIG_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const CONFIG_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONFIG_LOCK_BROKEN_STALE_AGE: Duration = Duration::from_secs(10);
const CONFIG_LOCK_PROC_START_RETRIES: usize = 3;
const CONFIG_LOCK_PROC_START_RETRY_DELAY: Duration = Duration::from_millis(250);
const CONFIG_LOCK_OWNER_FILE: &str = "owner.json";
const CONFIG_LOCK_QUARANTINE_PREFIX: &str = ".lock-quarantine";

#[derive(Clone, Copy)]
struct ConfigLockOptions {
    timeout: Duration,
    poll_interval: Duration,
    broken_stale_age: Duration,
}

const DEFAULT_CONFIG_LOCK_OPTIONS: ConfigLockOptions = ConfigLockOptions {
    timeout: CONFIG_LOCK_TIMEOUT,
    poll_interval: CONFIG_LOCK_POLL_INTERVAL,
    broken_stale_age: CONFIG_LOCK_BROKEN_STALE_AGE,
};

enum LockCleanupCondition {
    OwnerToken(String),
    MissingOwnerStale(Duration),
}

#[derive(Debug)]
struct ConfigLockGuard {
    lock_dir: PathBuf,
    owner_token: String,
}

struct ConfigLockOwner {
    pid: i32,
    proc_start: Option<String>,
    token: String,
}

/// `$LINGXI_CONFIG_DIR`, treating set-but-EMPTY as unset. DELIBERATE
/// divergence: in TS only `getGlobalClaudeFile`'s base (`env.ts:25`) is
/// `process.env.LINGXI_CONFIG_DIR || homedir()`-shaped (`""` falsy ⇒ home);
/// `getClaudeConfigHomeDir` (`envUtils.ts:8-14`) is `??`-shaped, so under
/// `LINGXI_CONFIG_DIR=""` TS resolves config-home to `""` — a cwd-RELATIVE
/// path, not `~/.lingxi`. That is pathological, not a behavior worth porting;
/// this port treats `""` as unset everywhere.
fn lingxi_config_dir_env() -> Option<PathBuf> {
    std::env::var_os(branding::CONFIG_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `getClaudeConfigHomeDir` (`envUtils.ts:7-14`): `$LINGXI_CONFIG_DIR` if
/// set (non-empty — see [`lingxi_config_dir_env`] for the empty-string
/// divergence), else `$HOME/.lingxi`. `None` when neither env var exists.
#[must_use]
pub fn lingxi_config_home() -> Option<PathBuf> {
    if let Some(dir) = lingxi_config_dir_env() {
        return Some(dir);
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(branding::DOT_DIR))
}

/// `getGlobalClaudeFile` (`env.ts:14-26`): legacy `<config-home>/.config.json`
/// when it exists, else `($LINGXI_CONFIG_DIR || $HOME)/.lingxi.json`.
///
/// The TS oauth filename suffix (`fileSuffixForOauthConfig()` →
/// `-custom-oauth`/`-local-oauth`/`-staging-oauth`) only applies under custom
/// OAuth env vars this port does not model (`anthropic-oauth` has no
/// `getOauthConfigType` substrate) — the default build resolves it to `""`,
/// so `.lingxi.json` is hardcoded here.
#[must_use]
pub fn global_config_path() -> Option<PathBuf> {
    let home = lingxi_config_home()?;
    let legacy = home.join(branding::LEGACY_GLOBAL_CONFIG_FILE);
    if legacy.exists() {
        return Some(legacy);
    }
    let base = lingxi_config_dir_env().or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    Some(base.join(branding::GLOBAL_CONFIG_FILE))
}

/// Errors from the `GlobalConfig` substrate. All callers treat any error as
/// "skip this write / skip this run" — never destructive.
#[derive(Debug)]
pub enum GlobalConfigError {
    /// The file exists but is not valid JSON (or not a JSON object). The
    /// migration runner skips the startup entirely rather than overwrite
    /// (stricter than TS, which falls back to defaults under a guard —
    /// documented divergence).
    Broken(String),
    /// I/O failure reading or writing.
    Io(std::io::Error),
}

impl std::fmt::Display for GlobalConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Broken(e) => write!(f, "invalid global config JSON: {e}"),
            Self::Io(e) => write!(f, "global config I/O error: {e}"),
        }
    }
}

impl std::error::Error for GlobalConfigError {}

/// Read `~/.lingxi.json` into a raw map. Missing file ⇒ empty map (TS
/// `getConfig` falls back to defaults; the typed getters below default per
/// key). Broken JSON ⇒ [`GlobalConfigError::Broken`].
pub fn read_map(path: &Path) -> Result<JsonMap, GlobalConfigError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(JsonMap::new()),
        Err(e) => return Err(GlobalConfigError::Io(e)),
    };
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|e| GlobalConfigError::Broken(e.to_string()))?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(GlobalConfigError::Broken(format!(
            "expected a JSON object, got {other}"
        ))),
    }
}

/// Materialize Claude's first-start pair before startup migrations run.
///
/// The gate is the JS truthiness of `firstStartTime`, not the independent
/// presence of `firstStartVersion`: a missing/null/empty/false/zero timestamp
/// starts a fresh pair and overwrites any stale version with the running
/// compatibility version. Once the timestamp is truthy, a missing version is
/// deliberately left missing — the oracle does not backfill it later.
pub fn ensure_first_start_metadata(
    path: &Path,
    first_start_time: &str,
    version: &str,
) -> Result<bool, GlobalConfigError> {
    let first_start_time = first_start_time.to_string();
    let version = version.to_string();
    save_map(path, move |mut map| {
        if map
            .get("firstStartTime")
            .is_some_and(crate::context::js_truthy)
        {
            return map;
        }
        map.insert(
            "firstStartTime".to_string(),
            Value::String(first_start_time),
        );
        map.insert("firstStartVersion".to_string(), Value::String(version));
        map
    })
}

/// Return the persisted installation `machineID`, creating/replacing it using
/// the same observable compatibility rule as Claude 2.1.245.
///
/// Any value that does not match `^[0-9a-f]{64}$` is replaced by a fresh
/// 64-character lowercase hexadecimal identifier.
#[must_use]
pub fn ensure_machine_id(path: &Path) -> String {
    if let Ok(map) = read_map(path) {
        if let Some(id) = map.get("machineID").and_then(Value::as_str) {
            if id.len() == 64
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return id.to_string();
            }
        }
    }
    let id = random_user_id();
    let to_store = id.clone();
    let _ = save_map(path, move |mut map| {
        map.insert("machineID".to_string(), Value::String(to_store));
        map
    });
    id
}

/// `saveGlobalConfig(prev => next)` (`config.ts:797-864`): read-modify-write.
/// The mutator's output is compared by VALUE — unchanged ⇒ zero write (the TS
/// same-reference skip). On write: strip legacy per-project `history` keys
/// (`removeProjectHistory`, `config.ts:966-989`) and write atomically
/// (same-dir tmp file + rename), pretty-printed with NO trailing newline
/// (TS `jsonStringify(filteredConfig, null, 2)`, `config.ts:1136` — only the
/// settings writer appends `\n`).
///
/// Returns `Ok(true)` if the file was written.
pub fn save_map(
    path: &Path,
    mutator: impl FnOnce(JsonMap) -> JsonMap,
) -> Result<bool, GlobalConfigError> {
    let target = resolve_write_target(path);
    let _lock = ConfigLockGuard::acquire(&target)?;
    let current = read_map(&target)?;
    let mut next = mutator(current.clone());
    if next == current {
        return Ok(false);
    }
    remove_project_history(&mut next);
    write_atomic(&target, &next)?;
    Ok(true)
}

/// `removeProjectHistory` (`config.ts:966-989`): drop the legacy `history`
/// key from every entry under `projects`. The `needsCleaning` gate in TS is
/// subsumed by `save_map`'s value-equality skip (we only reach here when a
/// write is happening anyway).
fn remove_project_history(map: &mut JsonMap) {
    if let Some(Value::Object(projects)) = map.get_mut("projects") {
        for (_path, proj) in projects.iter_mut() {
            if let Value::Object(p) = proj {
                p.remove("history");
            }
        }
    }
}

impl ConfigLockGuard {
    fn acquire(target: &Path) -> Result<Self, GlobalConfigError> {
        Self::acquire_with_options(target, DEFAULT_CONFIG_LOCK_OPTIONS)
    }

    fn acquire_with_options(
        target: &Path,
        options: ConfigLockOptions,
    ) -> Result<Self, GlobalConfigError> {
        let lock_dir = lock_dir_for_target(target);
        let deadline = Instant::now() + options.timeout;

        loop {
            if let Some(lock) = try_create_lock_dir(&lock_dir)? {
                return Ok(lock);
            }

            let cleanup = match read_lock_owner(&lock_dir)? {
                Some(owner) if !lock_owner_is_active(&owner) => {
                    Some(LockCleanupCondition::OwnerToken(owner.token))
                }
                Some(_) => None,
                None if lock_dir_is_stale(&lock_dir, options.broken_stale_age)? => Some(
                    LockCleanupCondition::MissingOwnerStale(options.broken_stale_age),
                ),
                None => None,
            };

            if let Some(cleanup) = cleanup {
                let _ = quarantine_lock_dir_if_matches(&lock_dir, cleanup)?;
                continue;
            }

            if Instant::now() >= deadline {
                return Err(GlobalConfigError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for global config lock {}",
                        lock_dir.display()
                    ),
                )));
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(options.poll_interval.min(remaining));
        }
    }
}

impl Drop for ConfigLockGuard {
    fn drop(&mut self) {
        let _ = quarantine_lock_dir_if_matches(
            &self.lock_dir,
            LockCleanupCondition::OwnerToken(self.owner_token.clone()),
        );
    }
}

pub(crate) fn resolve_write_target(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    match std::fs::read_link(path) {
        Ok(link) if link.is_absolute() => link,
        Ok(link) => dir.join(link),
        Err(_) => path.to_path_buf(),
    }
}

fn lock_dir_for_target(target: &Path) -> PathBuf {
    target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".{}.lock",
            target
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ))
}

fn lock_tmp_dir_for_target(lock_dir: &Path, owner: &ConfigLockOwner) -> PathBuf {
    lock_dir.with_file_name(format!(
        "{}.tmp-{}-{}",
        lock_dir
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        owner.pid,
        uuid::Uuid::new_v4().simple()
    ))
}

fn try_create_lock_dir(lock_dir: &Path) -> Result<Option<ConfigLockGuard>, GlobalConfigError> {
    let parent = lock_dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(GlobalConfigError::Io)?;

    let owner = current_lock_owner();
    let tmp_dir = lock_tmp_dir_for_target(lock_dir, &owner);
    create_secure_dir(&tmp_dir).map_err(GlobalConfigError::Io)?;
    if let Err(err) = write_lock_owner(&tmp_dir.join(CONFIG_LOCK_OWNER_FILE), &owner) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(err);
    }

    match std::fs::rename(&tmp_dir, lock_dir) {
        Ok(()) => Ok(Some(ConfigLockGuard {
            lock_dir: lock_dir.to_path_buf(),
            owner_token: owner.token,
        })),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists || lock_dir.exists() => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            Ok(None)
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            Err(GlobalConfigError::Io(err))
        }
    }
}

fn create_secure_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(path)
    }
}

fn current_lock_owner() -> ConfigLockOwner {
    let pid = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    ConfigLockOwner {
        pid,
        proc_start: read_process_start(pid),
        token: uuid::Uuid::new_v4().simple().to_string(),
    }
}

fn write_lock_owner(path: &Path, owner: &ConfigLockOwner) -> Result<(), GlobalConfigError> {
    let serialized = serde_json::to_string_pretty(&serde_json::json!({
        "pid": owner.pid,
        "procStart": owner.proc_start,
        "token": owner.token,
    }))
    .map_err(|e| GlobalConfigError::Broken(e.to_string()))?;
    write_secure(path, &serialized).map_err(GlobalConfigError::Io)
}

fn read_lock_owner(lock_dir: &Path) -> Result<Option<ConfigLockOwner>, GlobalConfigError> {
    let path = lock_dir.join(CONFIG_LOCK_OWNER_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(GlobalConfigError::Io(err)),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let Some(pid) = value
        .get("pid")
        .and_then(Value::as_i64)
        .and_then(|pid| i32::try_from(pid).ok())
    else {
        return Ok(None);
    };
    let Some(token) = value.get("token").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(Some(ConfigLockOwner {
        pid,
        proc_start: value
            .get("procStart")
            .and_then(Value::as_str)
            .map(str::to_owned),
        token: token.to_string(),
    }))
}

fn lock_owner_is_active(owner: &ConfigLockOwner) -> bool {
    if owner.pid <= 1 {
        return false;
    }
    if let Some(expected_start) = owner.proc_start.as_deref() {
        for attempt in 0..CONFIG_LOCK_PROC_START_RETRIES {
            if attempt > 0 {
                std::thread::sleep(CONFIG_LOCK_PROC_START_RETRY_DELAY);
            }
            if let Some(live_start) = read_process_start(owner.pid) {
                return live_start == expected_start;
            }
        }
    }
    process_is_alive(owner.pid)
}

fn lock_dir_is_stale(lock_dir: &Path, stale_age: Duration) -> Result<bool, GlobalConfigError> {
    let metadata = match std::fs::metadata(lock_dir) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(GlobalConfigError::Io(err)),
    };
    Ok(metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age >= stale_age))
}

fn remove_lock_dir(lock_dir: &Path) -> Result<(), GlobalConfigError> {
    match std::fs::remove_dir_all(lock_dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(GlobalConfigError::Io(err)),
    }
}

fn lock_quarantine_dir_for_token(lock_dir: &Path, token: &str) -> PathBuf {
    lock_dir.with_file_name(format!(
        "{}.{}-{}",
        lock_dir
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default(),
        CONFIG_LOCK_QUARANTINE_PREFIX,
        token
    ))
}

fn quarantine_lock_dir_if_matches(
    lock_dir: &Path,
    condition: LockCleanupCondition,
) -> Result<bool, GlobalConfigError> {
    let quarantine = match condition {
        LockCleanupCondition::OwnerToken(ref expected_token) => {
            let Some(owner) = read_lock_owner(lock_dir)? else {
                return Ok(false);
            };
            if owner.token != *expected_token {
                return Ok(false);
            }
            lock_quarantine_dir_for_token(lock_dir, expected_token)
        }
        LockCleanupCondition::MissingOwnerStale(stale_age) => {
            if read_lock_owner(lock_dir)?.is_some() || !lock_dir_is_stale(lock_dir, stale_age)? {
                return Ok(false);
            }
            lock_quarantine_dir_for_token(
                lock_dir,
                &format!("missing-owner-{}", uuid::Uuid::new_v4().simple()),
            )
        }
    };
    match std::fs::rename(lock_dir, &quarantine) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(GlobalConfigError::Io(err)),
    }

    let delete = match condition {
        LockCleanupCondition::OwnerToken(expected_token) => matches!(
            read_lock_owner(&quarantine)?,
            Some(owner) if owner.token == expected_token
        ),
        LockCleanupCondition::MissingOwnerStale(stale_age) => {
            read_lock_owner(&quarantine)?.is_none() && lock_dir_is_stale(&quarantine, stale_age)?
        }
    };
    if delete {
        remove_lock_dir(&quarantine).map(|()| true)
    } else {
        let _ = std::fs::rename(&quarantine, lock_dir);
        Ok(false)
    }
}

#[cfg(unix)]
fn read_process_start(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let start = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!start.is_empty()).then_some(start)
}

#[cfg(windows)]
fn read_process_start(pid: i32) -> Option<String> {
    if pid <= 1 {
        return None;
    }
    let script = format!(
        "(Get-Process -Id {pid} -ErrorAction Stop).StartTime.ToUniversalTime().ToString('o')"
    );
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let start = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!start.is_empty()).then_some(start)
}

#[cfg(not(any(unix, windows)))]
fn read_process_start(_pid: i32) -> Option<String> {
    None
}

#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "pid=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .output()
    else {
        return false;
    };
    output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

#[cfg(windows)]
fn process_is_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    let filter = format!("PID eq {pid}");
    let Ok(output) = std::process::Command::new("tasklist.exe")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split(',');
        let _image = fields.next();
        fields
            .next()
            .and_then(|field| field.trim().trim_matches('"').parse::<i32>().ok())
            == Some(pid)
    })
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(pid: i32) -> bool {
    pid > 1
}

/// Atomic global-config write: the port of `saveConfig`'s
/// `writeFileSyncAndFlush_DEPRECATED(file, jsonStringify(.., null, 2),
/// { mode: 0o600 })` call (`config.ts:1134-1141` → `file.ts:362-478`).
/// Used by BOTH write paths ([`save_map`] and [`save_project_config`]), so
/// both get the symlink / mode-preservation / tmp-cleanup behavior below.
fn write_atomic(path: &Path, map: &JsonMap) -> Result<(), GlobalConfigError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(GlobalConfigError::Io)?;
    let mut filtered = map.clone();
    filtered.retain(|key, value| !equals_global_default(key, value));
    let serialized = serde_json::to_string_pretty(&Value::Object(filtered))
        .map_err(|e| GlobalConfigError::Broken(e.to_string()))?;
    let target = resolve_write_target(path);
    if target.is_file() {
        if let Err(error) = backup_config_if_due(&target) {
            // Claude logs and continues when backup maintenance fails; a
            // backup problem must never turn a valid config update into a
            // failed startup.
            tracing::warn!(%error, path = %target.display(), "global config backup failed");
        }
    }
    let tmp = target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".{}.tmp-{}-{}",
            target
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default(),
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
    write_tmp_and_rename(&tmp, &target, &serialized).map_err(|e| {
        // Best-effort tmp cleanup on ANY failure (`file.ts:445-451`): the
        // tmp may or may not have been created — a failed remove is ignored,
        // exactly like the TS catch around `unlinkSync`.
        let _ = std::fs::remove_file(&tmp);
        GlobalConfigError::Io(e)
    })
}

fn equals_global_default(key: &str, value: &Value) -> bool {
    match (key, value) {
        (
            "numStartups"
            | "queuedCommandUpHintCount"
            | "memoryUsageCount"
            | "promptQueueUseCount"
            | "btwUseCount",
            Value::Number(number),
        ) => number.as_i64() == Some(0),
        ("messageIdleNotifThresholdMs", Value::Number(number)) => number.as_i64() == Some(60_000),
        ("theme", Value::String(text)) => text == "dark",
        ("preferredNotifChannel", Value::String(text)) => text == "auto",
        ("editorMode", Value::String(text)) => text == "normal",
        ("diffTool", Value::String(text)) => text == "auto",
        (
            "verbose"
            | "externalEditorContext"
            | "showMessageTimestamps"
            | "hasSeenTasksHint"
            | "hasUsedStash"
            | "hasUsedBackgroundTask"
            | "showExpandedTodos"
            | "briefTranscript"
            | "autoConnectIde"
            | "copyFullResponse"
            | "unpinOpus47LaunchEffort"
            | "unpinOpus48LaunchEffort"
            | "unpinFable5LaunchEffort",
            Value::Bool(false),
        ) => true,
        (
            "autoCompactEnabled"
            | "autoScrollEnabled"
            | "showTurnDuration"
            | "todoFeatureEnabled"
            | "autoInstallIdeExtension"
            | "fileCheckpointingEnabled"
            | "terminalProgressBarEnabled"
            | "respectGitignore",
            Value::Bool(true),
        ) => true,
        (
            "env" | "tipsHistory" | "cachedDynamicConfigs" | "cachedGrowthBookFeatures",
            Value::Object(object),
        ) => object.is_empty(),
        ("customApiKeyResponses", Value::Object(object)) => {
            object.len() == 2
                && object
                    .get("approved")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                && object
                    .get("rejected")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
        }
        _ => false,
    }
}

const CONFIG_BACKUP_MIN_INTERVAL: Duration = Duration::from_secs(60);
const CONFIG_BACKUP_RETAIN: usize = 5;

fn backup_config_if_due(target: &Path) -> std::io::Result<()> {
    let backups = config_backups_dir(target);
    create_secure_dir_all(&backups)?;
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{file_name}.backup.");
    let mut names = backup_names(&backups, &prefix)?;
    names.sort_unstable_by(|left, right| right.cmp(left));
    let newest_ms = names
        .first()
        .and_then(|name| name.strip_prefix(&prefix))
        .and_then(|stamp| stamp.parse::<u128>().ok())
        .unwrap_or(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    if now_ms.saturating_sub(newest_ms) >= CONFIG_BACKUP_MIN_INTERVAL.as_millis() {
        let name = format!("{prefix}{now_ms}");
        std::fs::copy(target, backups.join(&name))?;
        names.push(name);
        names.sort_unstable_by(|left, right| right.cmp(left));
    }
    for stale in names.into_iter().skip(CONFIG_BACKUP_RETAIN) {
        let _ = std::fs::remove_file(backups.join(stale));
    }
    Ok(())
}

fn create_secure_dir_all(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

fn backup_names(backups: &Path, prefix: &str) -> std::io::Result<Vec<String>> {
    Ok(std::fs::read_dir(backups)?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(prefix))
        .collect())
}

fn config_backups_dir(target: &Path) -> PathBuf {
    if global_config_path().as_deref() == Some(target) {
        if let Some(home) = lingxi_config_home() {
            return home.join("backups");
        }
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    if target.file_name() == Some(std::ffi::OsStr::new(branding::LEGACY_GLOBAL_CONFIG_FILE)) {
        parent.join("backups")
    } else {
        parent.join(branding::DOT_DIR).join("backups")
    }
}

/// The fallible tail of [`write_atomic`], isolated so its caller can clean up
/// the tmp file when ANY step here errors.
fn write_tmp_and_rename(tmp: &Path, target: &Path, contents: &str) -> std::io::Result<()> {
    // Existing-file mode wins over the new-file 0o600 (`file.ts:388-432`):
    // stat the target BEFORE writing, and re-apply its mode to the tmp file
    // before the rename (the TS `chmodSync(tempPath, targetMode)`). Only a
    // brand-new target keeps the 0o600 set at tmp creation.
    #[cfg(unix)]
    let existing_mode = std::fs::metadata(target)
        .ok()
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()));
    write_secure(tmp, contents)?;
    #[cfg(unix)]
    if let Some(mode) = existing_mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
    }
    std::fs::rename(tmp, target)
}

/// Write `contents` to a freshly created file with mode `0o600` on unix —
/// the `{ mode: 0o600 }` option of `config.ts:1134-1141`. `OpenOptions::mode`
/// only applies when the file is CREATED (like the TS `mode` option, which is
/// only set when the target didn't exist); [`write_tmp_and_rename`] always
/// calls this on a brand-new tmp path and separately re-applies an existing
/// target's mode afterwards. No-op mode-wise on non-unix.
fn write_secure(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(contents.as_bytes())
}

/// `getOrCreateUserID()` (`config.ts:1757-1766`): the stable per-install
/// identity stamped into the Anthropic request `metadata.user_id` as
/// `device_id`. Returns the persisted top-level `userID` when present,
/// otherwise generates `randomBytes(32).toString('hex')` (64 lowercase hex
/// chars), persists it to the global config, and returns it. A missing config
/// path (no HOME) yields a fresh non-persisted id so the caller always gets a
/// value; like TS, the generated id is returned regardless of the save outcome.
#[must_use]
pub fn get_or_create_user_id() -> String {
    match global_config_path() {
        Some(path) => get_or_create_user_id_at(&path),
        None => random_user_id(),
    }
}

/// Path-parameterized core of [`get_or_create_user_id`] (testable without
/// touching `$HOME`).
fn get_or_create_user_id_at(path: &Path) -> String {
    if let Ok(map) = read_map(path) {
        if let Some(uid) = map.get("userID").and_then(Value::as_str) {
            if !uid.is_empty() {
                return uid.to_string();
            }
        }
    }
    let uid = random_user_id();
    let to_store = uid.clone();
    let _ = save_map(path, move |mut m| {
        m.insert("userID".to_string(), Value::String(to_store));
        m
    });
    uid
}

/// `randomBytes(32).toString('hex')` — 32 random bytes as 64 lowercase hex
/// chars. Sourced from two v4 UUIDs (the workspace's CSPRNG-backed random
/// substrate); the handful of version/variant bits are fixed, but this is a
/// private per-install identifier never compared byte-for-byte.
fn random_user_id() -> String {
    let mut s = String::with_capacity(64);
    for uuid in [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()] {
        for byte in uuid.as_bytes() {
            use std::fmt::Write as _;
            let _ = write!(s, "{byte:02x}");
        }
    }
    s
}

/// `getProjectPathForConfig` (`config.ts:1588-1601`): the CANONICAL git root
/// of the directory — walk up looking for a `.git` entry (dir OR file), then
/// resolve a linked worktree's `.git` file through `gitdir:` → `commondir`
/// to the MAIN repo root (`findCanonicalGitRoot`, `git.ts:123-210`) so all
/// worktrees of a repo share one project key — else the canonicalized
/// directory itself; forward slashes for stable JSON keys.
#[must_use]
pub fn project_path_for_config(dir: &Path) -> String {
    let resolved = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut cur: Option<&Path> = Some(&resolved);
    while let Some(p) = cur {
        if p.join(".git").exists() {
            return resolve_canonical_root(p)
                .to_string_lossy()
                .replace('\\', "/");
        }
        cur = p.parent();
    }
    resolved.to_string_lossy().replace('\\', "/")
}

/// `resolveCanonicalRoot` (`git.ts:123-185`): for a regular repo (`.git` is a
/// directory) this is a no-op. For a linked worktree (`.git` is a file with a
/// `gitdir: <path>` pointer) follow `gitdir:` → `commondir` to the main
/// repo's working directory. Submodules (`.git` file, no `commondir`) and any
/// read/validation failure fall through to the input root, exactly as the TS
/// `catch` does. (The TS NFC-normalize is skipped per the module-level note.)
fn resolve_canonical_root(git_root: &Path) -> PathBuf {
    resolve_worktree_main_root(git_root).unwrap_or_else(|| git_root.to_path_buf())
}

/// The fallible body of [`resolve_canonical_root`]; `None` ⇒ keep `git_root`.
fn resolve_worktree_main_root(git_root: &Path) -> Option<PathBuf> {
    // In a worktree, `.git` is a file containing `gitdir: <path>`. In a
    // regular repo it is a directory: read fails (TS EISDIR) ⇒ fall through.
    let git_content = std::fs::read_to_string(git_root.join(".git")).ok()?;
    let pointer = git_content.trim().strip_prefix("gitdir:")?;
    // TS `resolve(gitRoot, pointer)` — relative pointers resolve against the
    // worktree root, lexically (no symlink traversal).
    let worktree_git_dir = lexical_resolve(git_root, Path::new(pointer.trim()));
    // `commondir` points at the shared .git dir, RELATIVE TO the worktree
    // gitdir. Submodules have no commondir (TS ENOENT) ⇒ fall through.
    let commondir_raw = std::fs::read_to_string(worktree_git_dir.join("commondir")).ok()?;
    let common_dir = lexical_resolve(&worktree_git_dir, Path::new(commondir_raw.trim()));
    // SECURITY (per TS): validate the structure matches `git worktree add`.
    // 1) worktreeGitDir must be a direct child of <commonDir>/worktrees/.
    if worktree_git_dir.parent()? != common_dir.join("worktrees") {
        return None;
    }
    // 2) <worktreeGitDir>/gitdir must point back to <gitRoot>/.git. TS
    //    realpaths the backlink, and realpaths the root DIRECTORY then joins
    //    `.git` (never realpathing the `.git` file itself).
    let backlink_raw = std::fs::read_to_string(worktree_git_dir.join("gitdir")).ok()?;
    let backlink = std::fs::canonicalize(backlink_raw.trim()).ok()?;
    if backlink != std::fs::canonicalize(git_root).ok()?.join(".git") {
        return None;
    }
    // Bare-repo worktrees: the common dir isn't a `.git` inside a working
    // directory — use the common dir itself as the stable identity.
    if common_dir.file_name() != Some(std::ffi::OsStr::new(".git")) {
        return Some(common_dir);
    }
    Some(common_dir.parent()?.to_path_buf())
}

/// Node `path.resolve(base, p)`: absolute `p` wins, else join onto `base`;
/// then normalize `.`/`..` LEXICALLY (no filesystem access), dropping any
/// `..` that would climb above the root. (Node would additionally prepend
/// `cwd()` if the result were still relative — unreachable here because
/// `base` is always absolute: the canonicalized git root or a path resolved
/// from it.)
fn lexical_resolve(base: &Path, p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `getCurrentProjectConfig` (`config.ts:1602-1623`): the `projects[<key>]`
/// sub-object, empty map when absent. (The TS `allowedTools`
/// string-coercion quirk is not ported — no Rust reader consumes it.)
pub fn get_project_config(path: &Path, project_key: &str) -> Result<JsonMap, GlobalConfigError> {
    let map = read_map(path)?;
    Ok(map
        .get("projects")
        .and_then(Value::as_object)
        .and_then(|p| p.get(project_key))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

/// `saveCurrentProjectConfig` (`config.ts:1625-1700`): mutate the
/// `projects[<key>]` sub-object in place (no history-strip on this path —
/// mirrors TS, whose project-save writes `projects` directly).
pub fn save_project_config(
    path: &Path,
    project_key: &str,
    mutator: impl FnOnce(JsonMap) -> JsonMap,
) -> Result<bool, GlobalConfigError> {
    let target = resolve_write_target(path);
    let _lock = ConfigLockGuard::acquire(&target)?;
    let current = read_map(&target)?;
    let current_proj = current
        .get("projects")
        .and_then(Value::as_object)
        .and_then(|p| p.get(project_key))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let next_proj = mutator(current_proj.clone());
    if next_proj == current_proj {
        return Ok(false);
    }
    let mut next = current;
    let projects = next
        .entry("projects")
        .or_insert_with(|| Value::Object(JsonMap::new()));
    if let Value::Object(projects) = projects {
        projects.insert(project_key.to_string(), Value::Object(next_proj));
    }
    write_atomic(&target, &next)?;
    Ok(true)
}

/// `checkHasTrustDialogAccepted` → `computeTrustDialogAccepted`
/// (`config.ts:705-743`): is `cwd`, or any of its ancestor directories,
/// recorded as trusted on disk? Returns `true` when `projects[<key>]` carries
/// a truthy `"hasTrustDialogAccepted"` for any of:
///   1. the git-root / canonical project key of `cwd`
///      (`getProjectPathForConfig`, the PRIMARY persistence location), then
///   2. each lexical ancestor of `cwd` (`cwd`, `cwd/..`, … to root), keyed the
///      way TS's `normalizePathForConfigKey(resolve(..))` walk does.
///
/// Two intentional divergences from this module's other readers, both
/// fail-safe-to-PROMPT (never falsely trust): a missing or **broken** config
/// (`read_map`/`get_project_config` `Err`) yields `false` here rather than
/// propagating — the trust check must degrade to "ask the user", never to a
/// crash or an implicit grant. The TS in-memory session-trust branch
/// (`getSessionTrustAccepted`, the `homedir()===cwd` case) IS modeled here as
/// the FIRST short-circuit — parity with `computeTrustDialogAccepted`
/// (`config.ts:705-711`), where session trust OR disk grants acceptance.
#[must_use]
pub fn check_has_trust_dialog_accepted(config_path: &Path, cwd: &Path) -> bool {
    // (0) Session-level (in-memory) trust, set when the dialog was accepted
    // while `cwd === homedir()` (`TrustDialog.tsx:174-175` →
    // `computeTrustDialogAccepted`'s first `if (getSessionTrustAccepted())`,
    // `config.ts:709-711`). Disk is never consulted once this is set.
    if get_session_trust_accepted() {
        return true;
    }

    // Canonicalize once (fall back to as-is), mirroring `project_path_for_config`
    // and TS's `resolve(getCwd())` before the lexical parent-walk.
    let resolved = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    // (1) The primary persistence key (git root or canonical cwd).
    if project_has_trust(config_path, &project_path_for_config(cwd)) {
        return true;
    }

    // (2) Lexical ancestor walk: `cwd`, its parent, … up to the filesystem
    // root. `Path::parent` is the lexical `resolve(currentPath, '..')` analogue
    // and stops returning when `parent == current` at the root (TS's loop
    // break), so no explicit equality guard is needed.
    let mut cur: Option<&Path> = Some(resolved.as_path());
    while let Some(p) = cur {
        let key = p.to_string_lossy().replace('\\', "/");
        if project_has_trust(config_path, &key) {
            return true;
        }
        cur = p.parent();
    }

    false
}

/// `config.projects?.[key]?.hasTrustDialogAccepted` truthiness, fail-safe to
/// `false` on any read/parse error. TS reads the value as truthy (`if
/// (projectConfig?.hasTrustDialogAccepted)`); the writer only ever stores the
/// boolean `true`, so a `Value::Bool(true)` is the faithful truthiness test.
fn project_has_trust(config_path: &Path, key: &str) -> bool {
    matches!(
        get_project_config(config_path, key),
        Ok(p) if p.get("hasTrustDialogAccepted") == Some(&Value::Bool(true))
    )
}

/// `config.projects?.[getProjectPathForConfig(cwd)]?.hasLingxiMdExternalIncludesApproved`
/// truthiness — whether the user has approved Managed/Project/Local LINGXI.md
/// files to `@import` paths OUTSIDE the working dir (claude-code
/// `hasLingxiMdExternalIncludesApproved`, claudemd.ts:826-846). Fail-safe to
/// `false` on any read/parse error; the writer only ever stores boolean `true`.
/// No ancestor walk (unlike trust): this is the EXACT project's approval.
#[must_use]
pub fn check_has_lingxi_md_external_includes_approved(config_path: &Path, cwd: &Path) -> bool {
    matches!(
        get_project_config(config_path, &project_path_for_config(cwd)),
        Ok(p) if p.get("hasLingxiMdExternalIncludesApproved") == Some(&Value::Bool(true))
    )
}

/// Whether the external-includes warning has already been answered for this
/// exact project. A declined warning is remembered so startup does not prompt
/// on every launch while external imports remain disabled.
#[must_use]
pub fn check_has_lingxi_md_external_includes_warning_shown(config_path: &Path, cwd: &Path) -> bool {
    matches!(
        get_project_config(config_path, &project_path_for_config(cwd)),
        Ok(p) if p.get("hasLingxiMdExternalIncludesWarningShown") == Some(&Value::Bool(true))
    )
}

/// Persist the external LINGXI.md import decision for this exact project while
/// preserving every sibling project field and unknown global-config key.
///
/// Both choices mark the warning as shown. Only an affirmative choice enables
/// imports outside the current working directory.
pub fn save_lingxi_md_external_includes_decision(
    config_path: &Path,
    cwd: &Path,
    approved: bool,
) -> Result<(), GlobalConfigError> {
    save_project_config(config_path, &project_path_for_config(cwd), |mut p| {
        p.insert(
            "hasLingxiMdExternalIncludesApproved".to_string(),
            Value::Bool(approved),
        );
        p.insert(
            "hasLingxiMdExternalIncludesWarningShown".to_string(),
            Value::Bool(true),
        );
        p
    })
    .map(|_wrote| ())
}

/// Persist trust for `cwd` (the `TrustDialog` "Yes, I trust this folder"
/// branch, which calls `saveCurrentProjectConfig({ hasTrustDialogAccepted:
/// true })` against `getProjectPathForConfig()` — `config.ts:717`,
/// `TrustDialog.tsx:272-277`). Writes the same project key the primary check
/// in [`check_has_trust_dialog_accepted`] reads first. PRESERVES every other
/// key in that project's map and every other project (delegated to
/// [`save_project_config`]).
pub fn mark_trust_dialog_accepted(config_path: &Path, cwd: &Path) -> Result<(), GlobalConfigError> {
    save_project_config(config_path, &project_path_for_config(cwd), |mut p| {
        p.insert("hasTrustDialogAccepted".to_string(), Value::Bool(true));
        p
    })
    .map(|_wrote| ())
}

/// Record a "Yes, I trust this folder" acceptance — the `onChange` accept
/// branch of `TrustDialog.tsx:162,174-177`:
/// - when `cwd === homedir()` ([`trust_homedir`]) acceptance is SESSION-ONLY
///   (in-memory [`set_session_trust_accepted`]`(true)`, NOT persisted to disk),
///   so hooks/features work this run without persisting trust for `$HOME`;
/// - otherwise persist via [`mark_trust_dialog_accepted`]
///   (`saveCurrentProjectConfig`), best-effort — a write failure must not crash
///   startup (the session is still trusted-this-run), so a persist error is
///   logged and swallowed, matching both CLI gates' prior behavior.
///
/// The `$HOME`/cwd comparison is the raw `homedir() === getCwd()` (no
/// canonicalization in claude-code). Shared by the stdio-REPL gate
/// (`repl::trust_gate`) and the TUI gate (`mode::trust_gate`) so the branch is
/// defined and tested once.
pub fn record_trust_accept(config_path: &Path, cwd: &Path) {
    if Some(cwd) == trust_homedir().as_deref() {
        set_session_trust_accepted(true);
    } else if let Err(e) = mark_trust_dialog_accepted(config_path, cwd) {
        tracing::warn!(error = %e, "mark_trust_dialog_accepted failed (ignored)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;
    use serde_json::json;

    fn write_test_lock_dir(lock_dir: &Path, owner: &ConfigLockOwner) {
        create_secure_dir(lock_dir).unwrap();
        write_lock_owner(&lock_dir.join(CONFIG_LOCK_OWNER_FILE), owner).unwrap();
    }

    fn live_test_lock_owner(token: &str) -> ConfigLockOwner {
        let pid = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        ConfigLockOwner {
            pid,
            proc_start: read_process_start(pid),
            token: token.to_string(),
        }
    }

    #[test]
    fn user_id_is_64_hex_persisted_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".lingxi.json");
        let first = get_or_create_user_id_at(&path);
        // `randomBytes(32).toString('hex')` → 64 lowercase hex chars.
        assert_eq!(first.len(), 64, "device id is 64 hex chars");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "device id is lowercase hex: {first}"
        );
        // Persisted under top-level `userID` and reused on the next call.
        assert_eq!(
            read_map(&path)
                .unwrap()
                .get("userID")
                .and_then(Value::as_str),
            Some(first.as_str())
        );
        assert_eq!(
            get_or_create_user_id_at(&path),
            first,
            "stable across calls"
        );
    }

    #[test]
    fn user_id_honors_existing_nonempty_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".lingxi.json");
        std::fs::write(&path, r#"{"userID":"preexisting-id","other":1}"#).unwrap();
        assert_eq!(get_or_create_user_id_at(&path), "preexisting-id");
        // An empty stored value is treated as absent → regenerated (64 hex).
        std::fs::write(&path, r#"{"userID":""}"#).unwrap();
        assert_eq!(get_or_create_user_id_at(&path).len(), 64);
    }

    #[test]
    fn config_home_prefers_claude_config_dir() {
        let _g = env_lock();
        std::env::set_var("LINGXI_CONFIG_DIR", "/tmp/cc-test-home");
        assert_eq!(
            lingxi_config_home(),
            Some(std::path::PathBuf::from("/tmp/cc-test-home"))
        );
        std::env::remove_var("LINGXI_CONFIG_DIR");
    }

    #[test]
    fn config_home_falls_back_to_home_dot_claude() {
        let _g = env_lock();
        std::env::remove_var("LINGXI_CONFIG_DIR");
        std::env::set_var("HOME", "/tmp/cc-test-h2");
        assert_eq!(
            lingxi_config_home(),
            Some(std::path::PathBuf::from("/tmp/cc-test-h2/.lingxi"))
        );
    }

    #[test]
    fn config_home_treats_empty_claude_config_dir_as_unset() {
        let _g = env_lock();
        // Deliberate divergence (see `lingxi_config_dir_env`): TS's
        // `??`-shaped getClaudeConfigHomeDir would use "" (cwd-relative);
        // this port treats "" as unset and falls back to ~/.lingxi.
        std::env::set_var("LINGXI_CONFIG_DIR", "");
        std::env::set_var("HOME", "/tmp/cc-test-h4");
        assert_eq!(
            lingxi_config_home(),
            Some(std::path::PathBuf::from("/tmp/cc-test-h4/.lingxi"))
        );
        assert_eq!(
            global_config_path(),
            Some(std::path::PathBuf::from("/tmp/cc-test-h4/.lingxi.json"))
        );
        std::env::remove_var("LINGXI_CONFIG_DIR");
    }

    #[test]
    fn global_path_prefers_legacy_config_json_when_present() {
        let _g = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LINGXI_CONFIG_DIR", tmp.path());
        std::fs::write(tmp.path().join(".config.json"), "{}").unwrap();
        assert_eq!(global_config_path(), Some(tmp.path().join(".config.json")));
        std::env::remove_var("LINGXI_CONFIG_DIR");
    }

    #[test]
    fn global_path_is_claude_json_under_config_dir_else_home() {
        let _g = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("LINGXI_CONFIG_DIR", tmp.path());
        assert_eq!(global_config_path(), Some(tmp.path().join(".lingxi.json")));
        std::env::remove_var("LINGXI_CONFIG_DIR");
        std::env::set_var("HOME", "/tmp/cc-test-h3");
        assert_eq!(
            global_config_path(),
            Some(std::path::PathBuf::from("/tmp/cc-test-h3/.lingxi.json"))
        );
    }

    use crate::test_support::temp_config;

    #[test]
    fn read_map_missing_file_is_empty() {
        let t = temp_config();
        assert!(read_map(&t.global).unwrap().is_empty());
    }

    #[test]
    fn read_map_broken_json_is_error() {
        let t = temp_config();
        std::fs::write(&t.global, "{ not json").unwrap();
        assert!(read_map(&t.global).is_err());
    }

    #[test]
    fn first_start_metadata_uses_timestamp_truthiness_as_its_only_gate() {
        let t = temp_config();
        ensure_first_start_metadata(&t.global, "2026-08-25T00:00:00.000Z", "2.1.245").unwrap();
        let first = read_map(&t.global).unwrap();
        assert_eq!(first["firstStartTime"], json!("2026-08-25T00:00:00.000Z"));
        assert_eq!(first["firstStartVersion"], json!("2.1.245"));

        std::fs::write(
            &t.global,
            r#"{"firstStartTime":"2000-01-01T00:00:00.000Z"}"#,
        )
        .unwrap();
        assert!(
            !ensure_first_start_metadata(&t.global, "2026-08-25T00:00:00.000Z", "2.1.245").unwrap()
        );
        let preserved = read_map(&t.global).unwrap();
        assert!(preserved.get("firstStartVersion").is_none());

        std::fs::write(
            &t.global,
            r#"{"firstStartTime":null,"firstStartVersion":"old"}"#,
        )
        .unwrap();
        ensure_first_start_metadata(&t.global, "2026-08-25T00:00:00.000Z", "2.1.245").unwrap();
        let replaced = read_map(&t.global).unwrap();
        assert_eq!(
            replaced["firstStartTime"],
            json!("2026-08-25T00:00:00.000Z")
        );
        assert_eq!(replaced["firstStartVersion"], json!("2.1.245"));
    }

    #[test]
    fn machine_id_requires_64_lowercase_hex_characters() {
        let t = temp_config();
        let created = ensure_machine_id(&t.global);
        assert_eq!(created.len(), 64);
        assert_eq!(read_map(&t.global).unwrap()["machineID"], json!(created));

        let existing = "a".repeat(64);
        std::fs::write(&t.global, json!({"machineID": existing}).to_string()).unwrap();
        assert_eq!(ensure_machine_id(&t.global), existing);

        for invalid in [
            String::new(),
            "legacy".to_string(),
            "A".repeat(64),
            "g".repeat(64),
        ] {
            std::fs::write(&t.global, json!({"machineID": &invalid}).to_string()).unwrap();
            let replaced = ensure_machine_id(&t.global);
            assert_eq!(replaced.len(), 64);
            assert!(replaced
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            assert_ne!(replaced, invalid);
        }
    }

    #[test]
    fn config_writes_backup_existing_bytes_but_not_a_new_file() {
        let t = temp_config();
        save_map(&t.global, |mut map| {
            map.insert("first".into(), json!(1));
            map
        })
        .unwrap();
        let backups = config_backups_dir(&t.global);
        assert!(!backups.exists());

        let original = std::fs::read_to_string(&t.global).unwrap();
        save_map(&t.global, |mut map| {
            map.insert("second".into(), json!(2));
            map
        })
        .unwrap();
        let names = backup_names(&backups, "claude.json.backup.").unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(
            std::fs::read_to_string(backups.join(&names[0])).unwrap(),
            original
        );

        // A second write inside the 60-second window reuses the same backup.
        save_map(&t.global, |mut map| {
            map.insert("third".into(), json!(3));
            map
        })
        .unwrap();
        assert_eq!(
            backup_names(&backups, "claude.json.backup.").unwrap().len(),
            1
        );
    }

    #[test]
    fn save_map_roundtrips_and_preserves_unknown_keys() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"zeta":1,"oauthAccount":{"id":"x"},"numStartups":42,"alpha":true}"#,
        )
        .unwrap();
        let wrote = save_map(&t.global, |mut m| {
            m.insert("migrationVersion".into(), serde_json::json!(11));
            m
        })
        .unwrap();
        assert!(wrote);
        let back = read_map(&t.global).unwrap();
        assert_eq!(back["zeta"], serde_json::json!(1));
        assert_eq!(back["oauthAccount"]["id"], serde_json::json!("x"));
        assert_eq!(back["numStartups"], serde_json::json!(42));
        assert_eq!(back["migrationVersion"], serde_json::json!(11));
        // preserve_order: original keys keep their relative order.
        let keys: Vec<&String> = back.keys().collect();
        assert!(
            keys.iter().position(|k| *k == "zeta").unwrap()
                < keys.iter().position(|k| *k == "alpha").unwrap()
        );
    }

    #[test]
    fn save_map_filters_only_values_equal_to_built_in_defaults() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"theme":"light","verbose":true,"unknown":{"x":1}}"#,
        )
        .unwrap();
        save_map(&t.global, |mut map| {
            map.insert("theme".into(), json!("dark"));
            map.insert("verbose".into(), json!(false));
            map.insert("autoCompactEnabled".into(), json!(true));
            map.insert("installMethod".into(), Value::Null);
            map
        })
        .unwrap();
        let back = read_map(&t.global).unwrap();
        assert!(back.get("theme").is_none());
        assert!(back.get("verbose").is_none());
        assert!(back.get("autoCompactEnabled").is_none());
        assert_eq!(back["unknown"], json!({"x": 1}));
        // `installMethod` defaults to JS `undefined`; an explicit JSON null is
        // not equal to that default and must survive.
        assert_eq!(back["installMethod"], Value::Null);
    }

    #[test]
    fn save_map_no_change_writes_nothing() {
        let t = temp_config();
        std::fs::write(&t.global, "{\"a\": 1}\n").unwrap();
        let before = std::fs::metadata(&t.global).unwrap().modified().unwrap();
        let wrote = save_map(&t.global, |m| m).unwrap();
        assert!(!wrote);
        let after = std::fs::metadata(&t.global).unwrap().modified().unwrap();
        assert_eq!(before, after, "file must be untouched");
    }

    #[test]
    fn save_map_broken_json_refuses_to_write() {
        let t = temp_config();
        std::fs::write(&t.global, "{ broken").unwrap();
        let res = save_map(&t.global, |mut m| {
            m.insert("x".into(), serde_json::json!(1));
            m
        });
        assert!(res.is_err());
        assert_eq!(std::fs::read_to_string(&t.global).unwrap(), "{ broken");
    }

    #[test]
    fn save_map_strips_legacy_project_history() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"projects":{"/p":{"history":["old"],"allowedTools":[]}}}"#,
        )
        .unwrap();
        save_map(&t.global, |mut m| {
            m.insert("migrationVersion".into(), serde_json::json!(11));
            m
        })
        .unwrap();
        let back = read_map(&t.global).unwrap();
        assert!(back["projects"]["/p"].get("history").is_none());
        assert!(back["projects"]["/p"].get("allowedTools").is_some());
    }

    #[test]
    fn project_config_get_and_save() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"projects":{"/proj":{"enabledMcpjsonServers":["a"]}}}"#,
        )
        .unwrap();
        let proj = get_project_config(&t.global, "/proj").unwrap();
        assert_eq!(proj["enabledMcpjsonServers"], serde_json::json!(["a"]));
        // unknown project → empty
        assert!(get_project_config(&t.global, "/other").unwrap().is_empty());

        save_project_config(&t.global, "/proj", |mut p| {
            p.remove("enabledMcpjsonServers");
            p
        })
        .unwrap();
        let proj = get_project_config(&t.global, "/proj").unwrap();
        assert!(proj.get("enabledMcpjsonServers").is_none());
    }

    #[test]
    fn concurrent_project_writers_preserve_both_updates() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"projects":{"/proj":{"base":true}}}"#).unwrap();

        let path = t.global.clone();
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let entered_worker = entered.clone();
        let worker = std::thread::spawn(move || {
            save_project_config(&path, "/proj", |mut p| {
                p.insert("first".into(), serde_json::json!(1));
                entered_worker.wait();
                std::thread::sleep(Duration::from_millis(150));
                p
            })
            .unwrap();
        });

        entered.wait();
        save_project_config(&t.global, "/proj", |mut p| {
            p.insert("second".into(), serde_json::json!(2));
            p
        })
        .unwrap();
        worker.join().unwrap();

        let proj = get_project_config(&t.global, "/proj").unwrap();
        assert_eq!(proj["base"], serde_json::json!(true));
        assert_eq!(proj["first"], serde_json::json!(1));
        assert_eq!(proj["second"], serde_json::json!(2));
    }

    #[test]
    fn stale_dead_lock_is_recovered_and_removed() {
        let t = temp_config();
        let target = resolve_write_target(&t.global);
        let lock_dir = lock_dir_for_target(&target);
        write_test_lock_dir(
            &lock_dir,
            &ConfigLockOwner {
                pid: 999_999,
                proc_start: Some("definitely-not-live".to_string()),
                token: "stale-owner".to_string(),
            },
        );

        save_project_config(&t.global, "/proj", |mut p| {
            p.insert("recovered".into(), serde_json::json!(true));
            p
        })
        .unwrap();

        let proj = get_project_config(&t.global, "/proj").unwrap();
        assert_eq!(proj["recovered"], serde_json::json!(true));
        assert!(!lock_dir.exists(), "stale lock must be cleaned up");
    }

    #[test]
    fn lock_is_released_after_read_error() {
        let t = temp_config();
        let lock_dir = lock_dir_for_target(&resolve_write_target(&t.global));
        std::fs::write(&t.global, "{ broken").unwrap();

        let res = save_map(&t.global, |mut m| {
            m.insert("x".into(), serde_json::json!(1));
            m
        });

        assert!(res.is_err());
        assert!(!lock_dir.exists(), "lock must not leak on read failure");
    }

    #[test]
    fn old_guard_does_not_delete_replacement_lock() {
        let t = temp_config();
        let target = resolve_write_target(&t.global);
        let guard = ConfigLockGuard::acquire(&target).unwrap();
        let replacement_owner = live_test_lock_owner("replacement-owner");

        std::fs::remove_dir_all(&guard.lock_dir).unwrap();
        write_test_lock_dir(&guard.lock_dir, &replacement_owner);

        drop(guard);

        let current = read_lock_owner(&lock_dir_for_target(&target))
            .unwrap()
            .unwrap();
        assert_eq!(current.token, replacement_owner.token);
    }

    #[test]
    fn stale_break_snapshot_does_not_delete_replacement_lock() {
        let t = temp_config();
        let target = resolve_write_target(&t.global);
        let lock_dir = lock_dir_for_target(&target);
        let stale_owner = ConfigLockOwner {
            pid: 999_999,
            proc_start: Some("dead-process".to_string()),
            token: "stale-snapshot".to_string(),
        };
        write_test_lock_dir(&lock_dir, &stale_owner);
        let stale_token = read_lock_owner(&lock_dir).unwrap().unwrap().token;

        std::fs::remove_dir_all(&lock_dir).unwrap();
        let replacement_owner = live_test_lock_owner("replacement-owner");
        write_test_lock_dir(&lock_dir, &replacement_owner);

        let cleaned = quarantine_lock_dir_if_matches(
            &lock_dir,
            LockCleanupCondition::OwnerToken(stale_token),
        )
        .unwrap();

        assert!(!cleaned, "replacement lock must not be quarantined");
        let current = read_lock_owner(&lock_dir).unwrap().unwrap();
        assert_eq!(current.token, replacement_owner.token);
    }

    #[test]
    fn live_lock_times_out_without_being_broken() {
        let t = temp_config();
        let target = resolve_write_target(&t.global);
        let lock_dir = lock_dir_for_target(&target);
        let guard = ConfigLockGuard::acquire_with_options(
            &target,
            ConfigLockOptions {
                timeout: Duration::from_secs(1),
                poll_interval: Duration::from_millis(25),
                broken_stale_age: Duration::from_millis(25),
            },
        )
        .unwrap();

        let started = Instant::now();
        let err = ConfigLockGuard::acquire_with_options(
            &target,
            ConfigLockOptions {
                timeout: Duration::from_millis(150),
                poll_interval: Duration::from_millis(25),
                broken_stale_age: Duration::from_millis(25),
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, GlobalConfigError::Io(ref io) if io.kind() == std::io::ErrorKind::TimedOut)
        );
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "acquire should wait for the timeout budget"
        );
        assert!(lock_dir.exists(), "live holder lock must not be broken");

        drop(guard);
        assert!(!lock_dir.exists(), "dropping the holder removes the lock");
    }

    #[test]
    fn project_key_git_root_else_cwd() {
        let t = temp_config();
        let repo = t.project.join("repo");
        let nested = repo.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let key = project_path_for_config(&nested);
        let canon = repo.canonicalize().unwrap();
        assert_eq!(key, canon.to_string_lossy().replace('\\', "/"));

        let bare = t.project.join("loose");
        std::fs::create_dir_all(&bare).unwrap();
        let key2 = project_path_for_config(&bare);
        assert_eq!(
            key2,
            bare.canonicalize()
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        );
    }

    /// Simulated `git worktree add` layout: the worktree's key must be the
    /// MAIN repo root (TS `findCanonicalGitRoot`), not the worktree dir.
    #[test]
    fn project_key_resolves_worktree_to_main_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main");
        let wt_git_dir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git_dir).unwrap();
        std::fs::write(wt_git_dir.join("commondir"), "../..\n").unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt_git_dir.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()),
        )
        .unwrap();

        let key = project_path_for_config(&wt);
        assert_eq!(key, main.to_string_lossy().replace('\\', "/"));

        // Nested dirs inside the worktree resolve to the same key.
        let nested = wt.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(project_path_for_config(&nested), key);
    }

    /// Submodule shape — `.git` file but NO `commondir` — keeps the
    /// directory's own root (TS falls through on ENOENT).
    #[test]
    fn project_key_submodule_git_file_keeps_own_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("sub");
        std::fs::create_dir_all(root.join("parent/.git/modules/sub")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join(".git"),
            format!(
                "gitdir: {}\n",
                root.join("parent/.git/modules/sub").display()
            ),
        )
        .unwrap();
        let key = project_path_for_config(&sub);
        assert_eq!(key, sub.to_string_lossy().replace('\\', "/"));
    }

    /// Negative worktree shape: the `commondir` pointer resolves somewhere
    /// that is NOT the parent of `<commonDir>/worktrees/<wt>` — the security
    /// check fails and the key falls back to the worktree dir itself.
    #[test]
    fn project_key_worktree_bad_commondir_parent_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main");
        let wt_git_dir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git_dir).unwrap();
        // Broken invariant: commondir points at an unrelated dir, so
        // `worktree_git_dir.parent() != <commonDir>/worktrees`.
        let elsewhere = root.join("elsewhere/.git");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            wt_git_dir.join("commondir"),
            format!("{}\n", elsewhere.display()),
        )
        .unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt_git_dir.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()),
        )
        .unwrap();

        let key = project_path_for_config(&wt);
        assert_eq!(key, wt.to_string_lossy().replace('\\', "/"));
    }

    /// Negative worktree shape: the `gitdir` backlink is missing or points at
    /// the wrong worktree — validation fails, key falls back to the worktree.
    #[test]
    fn project_key_worktree_bad_gitdir_backlink_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let main = root.join("main");
        let wt_git_dir = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git_dir).unwrap();
        std::fs::write(wt_git_dir.join("commondir"), "../..\n").unwrap();
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_git_dir.display()),
        )
        .unwrap();

        // (a) Missing backlink file entirely.
        let key = project_path_for_config(&wt);
        assert_eq!(key, wt.to_string_lossy().replace('\\', "/"));

        // (b) Backlink present but pointing at a DIFFERENT directory's .git.
        let other = root.join("other");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        std::fs::write(
            wt_git_dir.join("gitdir"),
            format!("{}\n", other.join(".git").display()),
        )
        .unwrap();
        let key = project_path_for_config(&wt);
        assert_eq!(key, wt.to_string_lossy().replace('\\', "/"));
    }

    #[cfg(unix)]
    #[test]
    fn save_map_creates_file_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let t = temp_config();
        save_map(&t.global, |mut m| {
            m.insert("a".into(), serde_json::json!(1));
            m
        })
        .unwrap();
        let mode = std::fs::metadata(&t.global).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // TS `jsonStringify(.., null, 2)` writes NO trailing newline.
        let content = std::fs::read_to_string(&t.global).unwrap();
        assert!(!content.ends_with('\n'));
    }

    /// `file.ts:388-432`: an EXISTING file keeps its permissions across the
    /// atomic rewrite — the 0o600 only applies to brand-new files.
    #[cfg(unix)]
    #[test]
    fn save_map_preserves_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let t = temp_config();
        std::fs::write(&t.global, "{}").unwrap();
        std::fs::set_permissions(&t.global, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_map(&t.global, |mut m| {
            m.insert("a".into(), serde_json::json!(1));
            m
        })
        .unwrap();
        let mode = std::fs::metadata(&t.global).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    /// `file.ts:369-383`: a symlinked global config is written THROUGH — the
    /// rename lands at the resolved destination, the symlink survives.
    #[cfg(unix)]
    #[test]
    fn save_map_writes_through_symlink() {
        let t = temp_config();
        let real = t.home.join("real-claude.json");
        std::fs::write(&real, r#"{"keep":true}"#).unwrap();
        std::os::unix::fs::symlink(&real, &t.global).unwrap();

        save_map(&t.global, |mut m| {
            m.insert("a".into(), serde_json::json!(1));
            m
        })
        .unwrap();

        // Link is still a symlink to the same target…
        assert!(std::fs::symlink_metadata(&t.global)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&t.global).unwrap(), real);
        // …and the TARGET file got the update (unknown keys preserved).
        let back = read_map(&real).unwrap();
        assert_eq!(back["keep"], serde_json::json!(true));
        assert_eq!(back["a"], serde_json::json!(1));
    }

    // ---- hasTrustDialogAccepted store (config.ts:705-743) ----

    /// Write `projects[<key>] = { "hasTrustDialogAccepted": true }` to the
    /// temp config, returning the key used (so callers can also seed siblings).
    fn seed_trust(global: &Path, key: &str) {
        std::fs::write(
            global,
            serde_json::to_string(&serde_json::json!({
                "projects": { key: { "hasTrustDialogAccepted": true } }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn trust_check_cwd_accepted_is_true() {
        let t = temp_config();
        // No `.git`, so the project key == the canonicalized cwd.
        let key = project_path_for_config(&t.project);
        seed_trust(&t.global, &key);
        assert!(check_has_trust_dialog_accepted(&t.global, &t.project));
    }

    #[test]
    fn trust_check_ancestor_accepted_is_true() {
        let t = temp_config();
        // Trust a PARENT dir; check a nested child → true (parent-walk).
        let child = t.project.join("a/b/c");
        std::fs::create_dir_all(&child).unwrap();
        let parent_key = project_path_for_config(&t.project);
        seed_trust(&t.global, &parent_key);
        assert!(check_has_trust_dialog_accepted(&t.global, &child));
    }

    #[test]
    fn trust_check_none_is_false() {
        // `env_lock` serializes against the session-flag tests (the in-memory
        // `SESSION_TRUST_ACCEPTED` short-circuits `check_has_trust_dialog_accepted`).
        let _g = env_lock();
        assert!(!get_session_trust_accepted(), "no session flag leaked in");
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"projects":{"/some/other/proj":{"hasTrustDialogAccepted":true}}}"#,
        )
        .unwrap();
        assert!(!check_has_trust_dialog_accepted(&t.global, &t.project));
    }

    #[test]
    fn trust_mark_then_check_is_true() {
        let t = temp_config();
        mark_trust_dialog_accepted(&t.global, &t.project).unwrap();
        assert!(check_has_trust_dialog_accepted(&t.global, &t.project));
    }

    #[test]
    fn trust_mark_preserves_sibling_key_and_sibling_project() {
        let t = temp_config();
        let key = project_path_for_config(&t.project);
        std::fs::write(
            &t.global,
            serde_json::to_string(&serde_json::json!({
                "numStartups": 7,
                "projects": {
                    key.clone(): { "allowedTools": ["Bash"] },
                    "/other/project": { "x": 1 },
                }
            }))
            .unwrap(),
        )
        .unwrap();

        mark_trust_dialog_accepted(&t.global, &t.project).unwrap();

        let back = read_map(&t.global).unwrap();
        // Trust set on our project…
        assert_eq!(
            back["projects"][&key]["hasTrustDialogAccepted"],
            serde_json::json!(true)
        );
        // …a sibling key inside our project's map survives…
        assert_eq!(
            back["projects"][&key]["allowedTools"],
            serde_json::json!(["Bash"])
        );
        // …a sibling PROJECT survives…
        assert_eq!(
            back["projects"]["/other/project"]["x"],
            serde_json::json!(1)
        );
        // …and a top-level unknown key survives.
        assert_eq!(back["numStartups"], serde_json::json!(7));
    }

    #[test]
    fn trust_check_corrupt_file_is_false() {
        let _g = env_lock();
        assert!(!get_session_trust_accepted(), "no session flag leaked in");
        let t = temp_config();
        std::fs::write(&t.global, "{ broken").unwrap();
        // Fail-safe-to-prompt: corrupt config must NOT panic and must be false.
        assert!(!check_has_trust_dialog_accepted(&t.global, &t.project));
    }

    #[test]
    fn trust_check_missing_file_is_false() {
        let _g = env_lock();
        assert!(!get_session_trust_accepted(), "no session flag leaked in");
        let t = temp_config();
        // `t.global` never created.
        assert!(!check_has_trust_dialog_accepted(&t.global, &t.project));
    }

    // ---- hasLingxiMdExternalIncludesApproved (claudemd.ts:826-846) ----

    /// Seed `projects[<key>] = { "hasLingxiMdExternalIncludesApproved": <v> }`.
    fn seed_external_approved(global: &Path, key: &str, v: bool) {
        std::fs::write(
            global,
            serde_json::to_string(&serde_json::json!({
                "projects": { key: { "hasLingxiMdExternalIncludesApproved": v } }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn external_includes_check_cwd_approved_is_true() {
        let t = temp_config();
        let key = project_path_for_config(&t.project);
        seed_external_approved(&t.global, &key, true);
        assert!(check_has_lingxi_md_external_includes_approved(
            &t.global, &t.project
        ));
    }

    #[test]
    fn external_includes_check_false_value_is_false() {
        let t = temp_config();
        let key = project_path_for_config(&t.project);
        // Stored `false` must read as not-approved (matches only boolean `true`).
        seed_external_approved(&t.global, &key, false);
        assert!(!check_has_lingxi_md_external_includes_approved(
            &t.global, &t.project
        ));
    }

    #[test]
    fn external_includes_check_no_ancestor_walk() {
        let t = temp_config();
        // Approve a PARENT dir; a nested child must NOT inherit (unlike trust,
        // this is the EXACT project's approval — no parent-walk).
        let child = t.project.join("a/b/c");
        std::fs::create_dir_all(&child).unwrap();
        let parent_key = project_path_for_config(&t.project);
        seed_external_approved(&t.global, &parent_key, true);
        assert!(!check_has_lingxi_md_external_includes_approved(
            &t.global, &child
        ));
    }

    #[test]
    fn external_includes_check_other_project_is_false() {
        let t = temp_config();
        std::fs::write(
            &t.global,
            r#"{"projects":{"/some/other/proj":{"hasLingxiMdExternalIncludesApproved":true}}}"#,
        )
        .unwrap();
        assert!(!check_has_lingxi_md_external_includes_approved(
            &t.global, &t.project
        ));
    }

    #[test]
    fn external_includes_check_corrupt_or_missing_is_false() {
        let t = temp_config();
        // Missing file → false (fail-safe to local-only).
        assert!(!check_has_lingxi_md_external_includes_approved(
            &t.global, &t.project
        ));
        // Corrupt file → false, no panic.
        std::fs::write(&t.global, "{ broken").unwrap();
        assert!(!check_has_lingxi_md_external_includes_approved(
            &t.global, &t.project
        ));
    }

    #[test]
    fn external_includes_decision_preserves_project_and_global_siblings() {
        let t = temp_config();
        let key = project_path_for_config(&t.project);
        std::fs::write(
            &t.global,
            serde_json::to_vec(&serde_json::json!({
                "numStartups": 9,
                "projects": {
                    key.clone(): { "allowedTools": ["Read"] },
                    "/other": { "keep": true }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        save_lingxi_md_external_includes_decision(&t.global, &t.project, false).unwrap();

        let back = read_map(&t.global).unwrap();
        assert_eq!(
            back["projects"][&key]["hasLingxiMdExternalIncludesApproved"],
            serde_json::json!(false)
        );
        assert_eq!(
            back["projects"][&key]["hasLingxiMdExternalIncludesWarningShown"],
            serde_json::json!(true)
        );
        assert_eq!(
            back["projects"][&key]["allowedTools"],
            serde_json::json!(["Read"])
        );
        assert_eq!(back["projects"]["/other"]["keep"], serde_json::json!(true));
        assert_eq!(back["numStartups"], serde_json::json!(9));
        assert!(check_has_lingxi_md_external_includes_warning_shown(
            &t.global, &t.project
        ));
    }

    #[test]
    fn external_includes_acceptance_sets_both_flags() {
        let t = temp_config();
        save_lingxi_md_external_includes_decision(&t.global, &t.project, true).unwrap();
        assert!(check_has_lingxi_md_external_includes_approved(
            &t.global, &t.project
        ));
        assert!(check_has_lingxi_md_external_includes_warning_shown(
            &t.global, &t.project
        ));
    }

    /// Session-level (in-memory) trust short-circuits the disk check —
    /// `computeTrustDialogAccepted`'s first branch (`config.ts:709-711`).
    /// `SESSION_TRUST_ACCEPTED` is a process-global `AtomicBool`; this test
    /// holds `env_lock()` so no other env/global-mutating test interleaves,
    /// and RESETS the flag to `false` before returning so it does not leak.
    #[test]
    fn trust_check_session_flag_short_circuits_disk() {
        let _g = env_lock();
        // Default is false: a fresh process flag does not grant trust on its
        // own (the missing-file path is exercised by `trust_check_missing_*`).
        assert!(!get_session_trust_accepted());

        let t = temp_config();
        // No disk entry at all.
        assert!(!check_has_trust_dialog_accepted(&t.global, &t.project));

        // Setting the session flag grants trust with NO disk entry.
        set_session_trust_accepted(true);
        assert!(check_has_trust_dialog_accepted(&t.global, &t.project));
        assert!(get_session_trust_accepted());

        // Reset so the process-global does not leak into other tests.
        set_session_trust_accepted(false);
        assert!(!get_session_trust_accepted());
        assert!(!check_has_trust_dialog_accepted(&t.global, &t.project));
    }

    /// `trust_homedir` sources `$HOME` the same way `lingxi_config_home` does
    /// (`std::env::var_os("HOME")`), so the dialog's `cwd === homedir()` check
    /// matches the rest of the crate.
    #[test]
    fn trust_homedir_resolves_home_env() {
        let _g = env_lock();
        std::env::set_var("HOME", "/tmp/cc-trust-home");
        assert_eq!(trust_homedir(), Some(PathBuf::from("/tmp/cc-trust-home")));
    }

    /// `record_trust_accept` with `cwd == $HOME` ⇒ SESSION-ONLY: the in-memory
    /// flag is set, the re-check returns true, but NOTHING is persisted to disk
    /// (parity `TrustDialog.tsx:174-175` — `setSessionTrustAccepted`, NOT
    /// `saveCurrentProjectConfig`). `env_lock` serializes the `$HOME` mutation
    /// AND the process-global `SESSION_TRUST_ACCEPTED`; the flag is reset to
    /// `false` before returning so it does not leak across tests.
    #[test]
    fn record_trust_accept_home_is_session_only_not_persisted() {
        let _g = env_lock();
        set_session_trust_accepted(false);

        let t = temp_config();
        // Point `$HOME` at the cwd so `trust_homedir() == cwd` (raw
        // `homedir() === getCwd()`).
        std::env::set_var("HOME", &t.project);

        record_trust_accept(&t.global, &t.project);

        // (a) In-memory flag set; the re-check now returns true…
        assert!(get_session_trust_accepted());
        assert!(check_has_trust_dialog_accepted(&t.global, &t.project));

        // (b) …but NOTHING was written to disk: clearing the flag, the disk-only
        // check is false and the global config file was never created.
        set_session_trust_accepted(false);
        assert!(
            !check_has_trust_dialog_accepted(&t.global, &t.project),
            "home-dir accept must NOT persist trust to disk"
        );
        assert!(
            !t.global.exists(),
            "no global config file should be written"
        );

        // Reset so the process-global does not leak.
        set_session_trust_accepted(false);
    }

    /// `record_trust_accept` with `cwd != $HOME` ⇒ persisted to disk exactly as
    /// today (the non-home `saveCurrentProjectConfig` branch); the session flag
    /// is NOT touched.
    #[test]
    fn record_trust_accept_non_home_persists_to_disk() {
        let _g = env_lock();
        set_session_trust_accepted(false);

        let t = temp_config();
        // `$HOME` is a DIFFERENT directory than cwd (a sibling of `t.project`).
        let home = t.project.parent().unwrap().join("not-the-cwd");
        std::env::set_var("HOME", &home);

        record_trust_accept(&t.global, &t.project);

        // Session flag untouched; trust IS on disk (survives a flag reset).
        assert!(!get_session_trust_accepted());
        assert!(
            check_has_trust_dialog_accepted(&t.global, &t.project),
            "non-home accept must persist trust to disk"
        );
        assert!(
            t.global.exists(),
            "non-home accept must write the config file"
        );
    }
}
