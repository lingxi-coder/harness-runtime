//! macOS managed-preferences input for Native `policySettings` composition.
//!
//! This module implements the byte-bound `Wjn()` reader from Claude Code
//! 2.1.295. It only discovers and converts the two plist files; typed policy
//! admission and source selection belong to `core::settings::native_policy`.
//! In particular, the reader retains stdout as source text and a parsed
//! UTF-16 JSON projection side by side. It never routes plist JSON through
//! `serde_json::Value`.
//! The Native domain `com.anthropic.claudecode` is the one approved branding
//! difference here; the product path uses `com.lingxi.code` from `branding`.
#![forbid(unsafe_code)]

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

const PLUTIL: &str = "/usr/bin/plutil";
const PLUTIL_CONVERT_PREFIX: [&str; 5] = ["-convert", "json", "-o", "-", "--"];
const PLUTIL_LINT_PREFIX: [&str; 3] = ["-lint", "-s", "--"];
const PLUTIL_TIMEOUT: Duration = Duration::from_millis(5_000);
const PLUTIL_CONVERT_MAX_BYTES: usize = 2 * 1024 * 1024;
// `execFile` uses its runtime default when Native omits `maxBuffer` for lint.
// Node's default is 1 MiB; keeping capture bounded preserves that behavior.
const PLUTIL_DEFAULT_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedPlistLabel {
    PerUser,
    Device,
}

impl ManagedPlistLabel {
    #[must_use]
    pub(super) const fn native_label(self) -> &'static str {
        match self {
            Self::PerUser => "per-user managed preferences",
            Self::Device => "device-level managed preferences",
        }
    }
}

/// One result emitted by the Native `Wjn()` plist reader.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ManagedPlistCandidate {
    pub(super) path: PathBuf,
    pub(super) label: ManagedPlistLabel,
    /// Native `qe(..., {userWritable})` provenance. The per-user path is
    /// user-writable; the device path is administrator-controlled.
    pub(super) user_writable: bool,
    /// `None` is Native `stdout:null`; `Some("")` remains distinct and stops
    /// `Wjn()` fallback even though it is not parseable JSON.
    pub(super) stdout: Option<String>,
    /// Source-preserving JSON value for valid stdout. Escaped lone UTF-16
    /// surrogates and property insertion order remain intact.
    pub(super) projection: Option<Utf16JsonProjection>,
    pub(super) unread_reason: Option<String>,
    pub(super) unread_errno: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct ManagedPlistSnapshot {
    /// Paths are ordered per-user then device and truncated through the first
    /// candidate with non-null stdout, exactly as Native `Wjn()` does.
    pub(super) candidates: Vec<ManagedPlistCandidate>,
    /// All non-absent read results before Native's first-non-null truncation.
    /// A later candidate may have completed concurrently but is not an input
    /// to policy selection once an earlier candidate returned stdout.
    observations: Vec<ManagedPlistCandidate>,
    /// Every candidate path considered by the reader, including absent files.
    /// This is kept separately because Native omits absent candidates from the
    /// `Wjn()` result, while policy provenance still needs the exact path and
    /// load state for both source slots.
    checked_paths: Vec<(PathBuf, ManagedPlistLabel, bool)>,
}

impl ManagedPlistSnapshot {
    /// Preserve reader order and source facts while handing candidates to the
    /// Core source-aware policy admission layer. `document_has_policy_content`
    /// remains unknown here; Core derives Native's `C0t` fallback from each
    /// unchanged projection. Invalid/absent stdout stays an unavailable
    /// document with its original reader diagnostic available on `self`.
    pub(super) fn native_policy_documents(
        &self,
    ) -> Vec<lingxi_core::settings::native_policy::NativePolicyDocument> {
        use lingxi_core::settings::native_policy::{
            NativePolicyDocument, NativePolicyLoadState, NativePolicyPlistFacts,
        };

        self.checked_paths
            .iter()
            .scan(false, |truncated, (path, _label, user_writable)| {
                if *truncated {
                    return None;
                }
                let candidate = self
                    .candidates
                    .iter()
                    .find(|candidate| candidate.path.as_path() == path.as_path());
                if let Some(candidate) = candidate {
                    *truncated = candidate.stdout.is_some();
                } else if self.observations.iter().any(|observed| {
                    observed.path.as_path() == path.as_path() && observed.stdout.is_some()
                }) {
                    // This completed after an earlier non-null stdout and was
                    // discarded by Native's `Wjn()` result truncation.
                    *truncated = true;
                    return None;
                }
                Some((path, *user_writable, candidate))
            })
            .map(|(path, user_writable, candidate)| {
                let load_state = match candidate {
                    Some(candidate) if candidate.projection.is_some() => {
                        NativePolicyLoadState::Loaded
                    }
                    Some(_) => NativePolicyLoadState::DidNotLoad,
                    None => NativePolicyLoadState::Absent,
                };
                NativePolicyDocument::parse_plist(
                    candidate.and_then(|candidate| candidate.projection.clone()),
                    NativePolicyPlistFacts {
                        path: path.clone(),
                        user_writable,
                        load_state,
                        document_has_policy_content: None,
                    },
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccessFailure {
    message: String,
    errno: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlutilStatus {
    Success,
    Exited(i32),
    SpawnError,
    Timeout,
    Killed,
    MaxBuffer,
    IoError,
}

impl PlutilStatus {
    /// Native `R(result)` accepts a successful exit or any normal exit with a
    /// known code; non-zero exits continue to the `-lint` disambiguation.
    const fn completed(self) -> bool {
        matches!(self, Self::Success | Self::Exited(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlutilResult {
    status: PlutilStatus,
    stdout: String,
}

#[async_trait]
trait ManagedPlistIo: Send + Sync {
    async fn access_read(&self, path: &Path) -> Result<(), AccessFailure>;

    async fn probe_open_read(&self, path: &Path) -> Result<(), AccessFailure>;

    async fn run_plutil(&self, args: Vec<OsString>, max_output_bytes: usize) -> PlutilResult;
}

/// Read the current host's macOS MDM plist candidates.
///
/// Other desktop platforms have no Native `Wjn()` MDM plist source. This is a
/// raw-source producer only; callers still need to pass each valid projection
/// through the Native policy admission API before selecting it.
#[cfg(target_os = "macos")]
pub(super) async fn read_native_mdm_plists() -> ManagedPlistSnapshot {
    let username = current_os_username();
    read_with_io(username.as_deref(), &SystemManagedPlistIo).await
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn read_native_mdm_plists() -> ManagedPlistSnapshot {
    ManagedPlistSnapshot::default()
}

/// Candidate file paths for the settings watcher. Watching parent directories
/// catches both file updates and a user-level plist created after startup.
#[cfg(target_os = "macos")]
pub(super) fn native_mdm_plist_paths() -> Vec<PathBuf> {
    let username = current_os_username();
    candidate_paths(username.as_deref())
        .into_iter()
        .map(|(path, _, _)| path)
        .collect()
}

#[cfg(not(target_os = "macos"))]
pub(super) fn native_mdm_plist_paths() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(target_os = "macos")]
pub(super) fn native_mdm_watch_dirs() -> Vec<PathBuf> {
    native_mdm_plist_paths()
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .fold(Vec::new(), |mut dirs, dir| {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
            dirs
        })
}

#[cfg(not(target_os = "macos"))]
pub(super) fn native_mdm_watch_dirs() -> Vec<PathBuf> {
    Vec::new()
}

/// Use the effective OS account database, matching `os.userInfo().username`.
/// Do not consult `USER`/`USERNAME`: those are caller-controlled environment
/// strings and can select a different per-user managed-preferences path.
#[cfg(target_os = "macos")]
fn current_os_username() -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::effective())
        .ok()
        .flatten()
        .map(|user| user.name)
        .filter(|username| !username.is_empty())
}

fn candidate_paths(username: Option<&str>) -> Vec<(PathBuf, ManagedPlistLabel, bool)> {
    let root = Path::new("/Library/Managed Preferences");
    let filename = format!("{}.plist", branding::MANAGED_PREFERENCES_DOMAIN);
    let mut paths = Vec::with_capacity(2);
    if let Some(username) = username.filter(|username| !username.is_empty()) {
        paths.push((
            root.join(username).join(&filename),
            ManagedPlistLabel::PerUser,
            true,
        ));
    }
    paths.push((root.join(filename), ManagedPlistLabel::Device, false));
    paths
}

async fn read_with_io(username: Option<&str>, io: &dyn ManagedPlistIo) -> ManagedPlistSnapshot {
    let paths = candidate_paths(username);
    let checked_paths = paths.clone();
    let mut candidates = if paths.len() == 2 {
        // `Wjn()` starts both user and device reads in one Promise.all. Keep
        // that even though the later non-null stdout truncates device results.
        let (user, device) = tokio::join!(read_one(io, &paths[0]), read_one(io, &paths[1]));
        vec![user, device]
    } else {
        vec![read_one(io, &paths[0]).await]
    }
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let observations = candidates.clone();

    if let Some(first_non_null) = candidates
        .iter()
        .position(|candidate| candidate.stdout.is_some())
    {
        candidates.truncate(first_non_null + 1);
    }

    ManagedPlistSnapshot {
        candidates,
        observations,
        checked_paths,
    }
}

async fn read_one(
    io: &dyn ManagedPlistIo,
    (path, label, user_writable): &(PathBuf, ManagedPlistLabel, bool),
) -> Option<ManagedPlistCandidate> {
    if let Err(error) = io.access_read(path).await {
        if matches!(error.errno.as_deref(), Some("ENOENT" | "ENOTDIR")) {
            return None;
        }
        return Some(candidate(
            path.clone(),
            *label,
            *user_writable,
            None,
            Some(error.message),
            error.errno,
        ));
    }

    let convert = io
        .run_plutil(
            plutil_args(&PLUTIL_CONVERT_PREFIX, path),
            PLUTIL_CONVERT_MAX_BYTES,
        )
        .await;
    if !convert.status.completed() {
        return Some(candidate(
            path.clone(),
            *label,
            *user_writable,
            None,
            Some("plutil did not finish converting it".to_owned()),
            None,
        ));
    }
    if convert.status == PlutilStatus::Success {
        return Some(candidate(
            path.clone(),
            *label,
            *user_writable,
            Some(convert.stdout),
            None,
            None,
        ));
    }

    let lint = io
        .run_plutil(
            plutil_args(&PLUTIL_LINT_PREFIX, path),
            PLUTIL_DEFAULT_MAX_BYTES,
        )
        .await;
    if !lint.status.completed() {
        return Some(candidate(
            path.clone(),
            *label,
            *user_writable,
            None,
            Some("plutil did not finish checking it".to_owned()),
            None,
        ));
    }
    if lint.status == PlutilStatus::Success {
        return Some(candidate(
            path.clone(),
            *label,
            *user_writable,
            None,
            Some("it holds a value plutil cannot convert to JSON (a date or data value)".into()),
            None,
        ));
    }

    // Native rechecks readability after both plutil commands fail. A vanished
    // file is omitted; a readable file yields stdout:"" (non-null), while an
    // access error is retained as a null-stdout diagnostic.
    match io.probe_open_read(path).await {
        Ok(()) => Some(candidate(
            path.clone(),
            *label,
            *user_writable,
            Some(String::new()),
            None,
            None,
        )),
        Err(error) if matches!(error.errno.as_deref(), Some("ENOENT" | "ENOTDIR")) => None,
        Err(error) => Some(candidate(
            path.clone(),
            *label,
            *user_writable,
            None,
            Some(error.message),
            error.errno,
        )),
    }
}

fn candidate(
    path: PathBuf,
    label: ManagedPlistLabel,
    user_writable: bool,
    stdout: Option<String>,
    unread_reason: Option<String>,
    unread_errno: Option<String>,
) -> ManagedPlistCandidate {
    let projection = stdout
        .as_deref()
        .and_then(|source| Utf16JsonProjection::parse(source).ok());
    ManagedPlistCandidate {
        path,
        label,
        user_writable,
        stdout,
        projection,
        unread_reason,
        unread_errno,
    }
}

fn plutil_args(prefix: &[&str], path: &Path) -> Vec<OsString> {
    prefix
        .iter()
        .map(OsString::from)
        .chain(std::iter::once(path.as_os_str().to_owned()))
        .collect()
}

#[cfg(unix)]
struct SystemManagedPlistIo;

#[cfg(unix)]
#[async_trait]
impl ManagedPlistIo for SystemManagedPlistIo {
    async fn access_read(&self, path: &Path) -> Result<(), AccessFailure> {
        nix::unistd::access(path, nix::unistd::AccessFlags::R_OK)
            .map_err(|errno| access_failure(path, errno))
    }

    async fn probe_open_read(&self, path: &Path) -> Result<(), AccessFailure> {
        use std::io::Read;

        let mut file =
            std::fs::File::open(path).map_err(|error| io_probe_failure(path, error, "open"))?;
        let mut byte = [0_u8; 1];
        file.read(&mut byte)
            .map_err(|error| io_probe_failure(path, error, "read"))?;
        Ok(())
    }

    async fn run_plutil(&self, args: Vec<OsString>, max_output_bytes: usize) -> PlutilResult {
        run_plutil(args, max_output_bytes).await
    }
}

#[cfg(unix)]
fn access_failure(path: &Path, errno: nix::errno::Errno) -> AccessFailure {
    let name = errno_name(errno);
    let message = match name {
        Some(name) => format!(
            "{name}: {}, access '{}'",
            errno.desc().to_ascii_lowercase(),
            path.display()
        ),
        None => format!("{}: access '{}'", errno.desc(), path.display()),
    };
    AccessFailure {
        message,
        errno: name.map(str::to_owned),
    }
}

#[cfg(unix)]
fn io_probe_failure(path: &Path, error: std::io::Error, syscall: &str) -> AccessFailure {
    let errno = error.raw_os_error().map(nix::errno::Errno::from_i32);
    let name = errno.and_then(errno_name);
    let message = match (errno, name) {
        (Some(errno), Some(name)) => format!(
            "{name}: {}, {syscall} '{}'",
            errno.desc().to_ascii_lowercase(),
            path.display()
        ),
        (Some(errno), None) => format!("{}: {syscall} '{}'", errno.desc(), path.display()),
        (None, _) => format!("{error}, {syscall} '{}'", path.display()),
    };
    AccessFailure {
        message,
        errno: name.map(str::to_owned),
    }
}

#[cfg(unix)]
fn errno_name(errno: nix::errno::Errno) -> Option<&'static str> {
    match errno {
        nix::errno::Errno::ENOENT => Some("ENOENT"),
        nix::errno::Errno::ENOTDIR => Some("ENOTDIR"),
        nix::errno::Errno::EACCES => Some("EACCES"),
        nix::errno::Errno::EPERM => Some("EPERM"),
        _ => None,
    }
}

async fn run_plutil(args: Vec<OsString>, max_output_bytes: usize) -> PlutilResult {
    let mut child = match Command::new(PLUTIL)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => {
            return PlutilResult {
                status: PlutilStatus::SpawnError,
                stdout: String::new(),
            }
        }
    };

    let Some(stdout) = child.stdout.take() else {
        terminate_child(&mut child).await;
        return PlutilResult {
            status: PlutilStatus::IoError,
            stdout: String::new(),
        };
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child(&mut child).await;
        return PlutilResult {
            status: PlutilStatus::IoError,
            stdout: String::new(),
        };
    };

    let result = tokio::time::timeout(PLUTIL_TIMEOUT, async {
        let wait = async { child.wait().await.map_err(|_| CaptureError::Io) };
        tokio::try_join!(
            read_capped(stdout, max_output_bytes),
            read_capped(stderr, max_output_bytes),
            wait
        )
    })
    .await;

    match result {
        Ok(Ok((stdout, _stderr, status))) => PlutilResult {
            status: match status.code() {
                Some(0) => PlutilStatus::Success,
                Some(code) => PlutilStatus::Exited(code),
                None => PlutilStatus::Killed,
            },
            // Node execFile uses UTF-8 decoding with replacement for malformed
            // byte sequences; `from_utf8_lossy` has the same boundary.
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
        },
        Ok(Err(CaptureError::MaxBuffer)) => {
            terminate_child(&mut child).await;
            PlutilResult {
                status: PlutilStatus::MaxBuffer,
                stdout: String::new(),
            }
        }
        Ok(Err(CaptureError::Io)) => {
            terminate_child(&mut child).await;
            PlutilResult {
                status: PlutilStatus::IoError,
                stdout: String::new(),
            }
        }
        Err(_) => {
            terminate_child(&mut child).await;
            PlutilResult {
                status: PlutilStatus::Timeout,
                stdout: String::new(),
            }
        }
    }
}

async fn read_capped<R>(mut reader: R, max_bytes: usize) -> Result<Vec<u8>, CaptureError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(max_bytes.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| CaptureError::Io)?;
        if read == 0 {
            return Ok(output);
        }
        if read > max_bytes.saturating_sub(output.len()) {
            return Err(CaptureError::MaxBuffer);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureError {
    MaxBuffer,
    Io,
}

async fn terminate_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeManagedPlistIo {
        access_failures: HashMap<PathBuf, AccessFailure>,
        probe_failures: HashMap<PathBuf, AccessFailure>,
        results: HashMap<(PathBuf, &'static str), PlutilResult>,
        calls: Mutex<Vec<(PathBuf, Vec<OsString>, usize)>>,
    }

    impl FakeManagedPlistIo {
        fn access_error(path: PathBuf, errno: &str, message: String) -> Self {
            Self {
                access_failures: HashMap::from([(
                    path,
                    AccessFailure {
                        message,
                        errno: Some(errno.to_owned()),
                    },
                )]),
                ..Self::default()
            }
        }

        fn probe_error(path: PathBuf, errno: &str, message: String) -> Self {
            Self {
                probe_failures: HashMap::from([(
                    path,
                    AccessFailure {
                        message,
                        errno: Some(errno.to_owned()),
                    },
                )]),
                ..Self::default()
            }
        }

        fn add_result(
            &mut self,
            path: &Path,
            stage: &'static str,
            status: PlutilStatus,
            stdout: &str,
        ) {
            self.results.insert(
                (path.to_path_buf(), stage),
                PlutilResult {
                    status,
                    stdout: stdout.to_owned(),
                },
            );
        }

        fn called_paths(&self) -> Vec<PathBuf> {
            self.calls
                .lock()
                .expect("fake call list")
                .iter()
                .map(|(path, _, _)| path.clone())
                .collect()
        }
    }

    #[async_trait]
    impl ManagedPlistIo for FakeManagedPlistIo {
        async fn access_read(&self, path: &Path) -> Result<(), AccessFailure> {
            self.access_failures.get(path).cloned().map_or(Ok(()), Err)
        }

        async fn probe_open_read(&self, path: &Path) -> Result<(), AccessFailure> {
            self.probe_failures.get(path).cloned().map_or(Ok(()), Err)
        }

        async fn run_plutil(&self, args: Vec<OsString>, max_output_bytes: usize) -> PlutilResult {
            let path = PathBuf::from(args.last().expect("plist path arg"));
            let stage = if args.first().is_some_and(|arg| arg == "-convert") {
                "-convert"
            } else {
                "-lint"
            };
            self.calls
                .lock()
                .expect("fake call list")
                .push((path.clone(), args, max_output_bytes));
            self.results
                .get(&(path, stage))
                .cloned()
                .unwrap_or(PlutilResult {
                    status: PlutilStatus::SpawnError,
                    stdout: String::new(),
                })
        }
    }

    fn expected_paths(username: Option<&str>) -> Vec<PathBuf> {
        candidate_paths(username)
            .into_iter()
            .map(|(path, _, _)| path)
            .collect()
    }

    #[tokio::test]
    async fn source_oracle_user_empty_stdout_blocks_device_fallback() {
        // Independently pinned by policy-oracle/two_input_oracle.cjs from
        // Wjn @ 188301950 (chunk SHA-256
        // 4666a180faee6eec8795f33f5333c6d538ea484f2d979cf68ac854eb342e1067)
        // and zt @ 188318324 in artifact
        // 0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70.
        let paths = expected_paths(Some("alice"));
        let mut io = FakeManagedPlistIo::default();
        io.add_result(&paths[0], "-convert", PlutilStatus::Success, "{}");
        io.add_result(
            &paths[1],
            "-convert",
            PlutilStatus::Success,
            r#"{"allowManagedHooksOnly":true}"#,
        );

        let snapshot = read_with_io(Some("alice"), &io).await;
        assert_eq!(snapshot.candidates.len(), 1);
        assert_eq!(snapshot.candidates[0].path, paths[0]);
        assert_eq!(snapshot.candidates[0].label, ManagedPlistLabel::PerUser);
        assert!(snapshot.candidates[0].user_writable);
        assert_eq!(snapshot.candidates[0].stdout.as_deref(), Some("{}"));
        assert_eq!(
            snapshot.candidates[0].projection,
            Utf16JsonProjection::parse("{}").ok()
        );
        assert_eq!(
            io.called_paths(),
            paths,
            "Wjn starts both reads before truncation"
        );
    }

    #[tokio::test]
    async fn source_oracle_unreadable_user_keeps_diagnostic_and_device_policy() {
        // This is the second fixed two-input oracle case: user stdout is null,
        // so the device candidate remains reachable with its raw JSON source.
        let paths = expected_paths(Some("alice"));
        let unread_reason = format!("EACCES: permission denied, access '{}'", paths[0].display());
        let mut io =
            FakeManagedPlistIo::access_error(paths[0].clone(), "EACCES", unread_reason.clone());
        io.add_result(
            &paths[1],
            "-convert",
            PlutilStatus::Success,
            r#"{"allowManagedHooksOnly":true}"#,
        );

        let snapshot = read_with_io(Some("alice"), &io).await;
        assert_eq!(snapshot.candidates.len(), 2);
        let user = &snapshot.candidates[0];
        assert_eq!(user.path, paths[0]);
        assert!(user.user_writable);
        assert_eq!(user.stdout, None);
        assert_eq!(user.unread_reason.as_deref(), Some(unread_reason.as_str()));
        assert_eq!(user.unread_errno.as_deref(), Some("EACCES"));
        let device = &snapshot.candidates[1];
        assert_eq!(device.path, paths[1]);
        assert!(!device.user_writable);
        assert_eq!(
            device
                .projection
                .as_ref()
                .and_then(|raw| raw.get_property("allowManagedHooksOnly"))
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn no_os_username_reads_only_the_device_path() {
        let paths = expected_paths(None);
        assert_eq!(paths.len(), 1);
        assert_eq!(
            paths[0],
            PathBuf::from(format!(
                "/Library/Managed Preferences/{}.plist",
                branding::MANAGED_PREFERENCES_DOMAIN
            ))
        );
        let mut io = FakeManagedPlistIo::default();
        io.add_result(
            &paths[0],
            "-convert",
            PlutilStatus::Success,
            r#"{"allowManagedHooksOnly":true}"#,
        );
        let snapshot = read_with_io(None, &io).await;
        assert_eq!(snapshot.candidates.len(), 1);
        assert_eq!(snapshot.candidates[0].label, ManagedPlistLabel::Device);
        assert!(!snapshot.candidates[0].user_writable);
    }

    #[tokio::test]
    async fn lint_success_marks_non_json_plist_as_null_stdout() {
        let path = expected_paths(None).remove(0);
        let mut io = FakeManagedPlistIo::default();
        io.add_result(&path, "-convert", PlutilStatus::Exited(1), "");
        io.add_result(&path, "-lint", PlutilStatus::Success, "");

        let snapshot = read_with_io(None, &io).await;
        assert_eq!(snapshot.candidates.len(), 1);
        let candidate = &snapshot.candidates[0];
        assert_eq!(candidate.stdout, None);
        assert_eq!(
            candidate.unread_reason.as_deref(),
            Some("it holds a value plutil cannot convert to JSON (a date or data value)")
        );
        let calls = io.calls.lock().expect("fake call list");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].1.first().and_then(|arg| arg.to_str()),
            Some("-convert")
        );
        assert_eq!(calls[0].2, PLUTIL_CONVERT_MAX_BYTES);
        assert_eq!(
            calls[0].1,
            ["-convert", "json", "-o", "-", "--"]
                .into_iter()
                .map(OsString::from)
                .chain(std::iter::once(path.as_os_str().to_owned()))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            calls[1].1.first().and_then(|arg| arg.to_str()),
            Some("-lint")
        );
        assert_eq!(calls[1].2, PLUTIL_DEFAULT_MAX_BYTES);
    }

    #[tokio::test]
    async fn convert_max_buffer_failure_stays_null_and_does_not_lint() {
        let path = expected_paths(None).remove(0);
        let mut io = FakeManagedPlistIo::default();
        io.add_result(&path, "-convert", PlutilStatus::MaxBuffer, "");

        let snapshot = read_with_io(None, &io).await;
        assert_eq!(snapshot.candidates.len(), 1);
        assert_eq!(snapshot.candidates[0].stdout, None);
        assert_eq!(
            snapshot.candidates[0].unread_reason.as_deref(),
            Some("plutil did not finish converting it")
        );
        assert_eq!(io.calls.lock().expect("fake call list").len(), 1);
    }

    #[tokio::test]
    async fn raw_projection_preserves_lone_surrogate_and_property_order() {
        let raw = r#"{"second":"\ud800","first":2}"#;
        let candidate = candidate(
            PathBuf::from("/Library/Managed Preferences/policy.plist"),
            ManagedPlistLabel::Device,
            false,
            Some(raw.to_owned()),
            None,
            None,
        );
        assert_eq!(candidate.stdout.as_deref(), Some(raw));
        let projection = candidate.projection.expect("valid JSON source projection");
        assert_eq!(projection.string_units("/second"), Some(vec![0xD800]));
        assert_eq!(projection.number_at("/first"), Some(2.0));
        assert_eq!(
            projection
                .object_entries()
                .unwrap()
                .into_iter()
                .map(|entry| entry.display_key.to_owned())
                .collect::<Vec<_>>(),
            vec!["second".to_owned(), "first".to_owned()]
        );
    }
}
