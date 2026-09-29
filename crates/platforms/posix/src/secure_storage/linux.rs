//! Linux Secret Service backend for [`SecureStorage`], shelling out to the
//! `libsecret` `secret-tool` CLI.
//!
//! claude-code has no Linux secure-storage backend today — its
//! `src/utils/secureStorage/index.ts` falls back to plaintext under a
//! `// TODO: add libsecret support for Linux` comment. This module realizes
//! that TODO by mirroring the macOS sibling ([`super::macos`]) exactly,
//! substituting the `security` CLI with `secret-tool`:
//! - Service name from [`super::helpers::full_service_name`] (byte-identical
//!   to the macOS layout, so the same `(service, account)` key resolves
//!   across hosts).
//! - JSON payload written whole to `secret-tool store`'s stdin. Unlike the
//!   macOS `security -i` path there is no `fgets()`/`BUFSIZ` limit and no
//!   hex-encode trick: `libsecret` reads the secret value from a pipe, so the
//!   payload never appears in argv and needs no length-checked fallback.
//! - No read cache: claude-code's Linux path is uncached plaintext, so a 1:1
//!   port keeps reads direct (the macOS cache exists only because that TS
//!   path has a `keychainCacheState`; the Linux TS path has none).
//! - `list` is not supported — claude-code's API never issues prefix queries.
//!   Returns [`SecureStorageError::BackendUnavailable`], mirroring
//!   [`super::macos`].

use crate::secure_storage::helpers::full_service_name;
use async_trait::async_trait;
use lingxi_core::host::{SecureStorage, SecureStorageBackend, SecureStorageError};
use lingxi_core::types::SecureStorageData;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Linux Secret Service backend (`GNOME` Keyring / `KWallet` via
/// `libsecret`).
///
/// Construct via [`LinuxSecretStorage::new`]. The `user` field is the
/// `account` attribute on each stored item (typically `$USER`); the
/// `config_dir` drives the per-config-directory dir-hash service-name suffix,
/// identical to the macOS backend.
pub struct LinuxSecretStorage {
    user: String,
    config_dir: PathBuf,
    default_config_dir: PathBuf,
    oauth_suffix: String,
}

impl LinuxSecretStorage {
    /// Construct a new `libsecret`-backed store.
    ///
    /// `user` is the `account` attribute (claude-code uses
    /// `process.env.USER || userInfo().username`).
    /// `config_dir` is the user's claude config directory (`~/.claude` or
    /// whatever `LINGXI_CONFIG_DIR` overrides it to).
    /// `default_config_dir` is what claude-code calls the "default"
    /// `~/.claude` — passed in so the dir-hash discriminator can compare.
    /// `oauth_suffix` mirrors claude-code's `OAUTH_FILE_SUFFIX`; pass `""`
    /// for the standard build.
    ///
    /// # Errors
    /// Returns [`SecureStorageError::BackendUnavailable`] when the
    /// `secret-tool` CLI is not on `$PATH` (the caller can then fall back to
    /// plaintext).
    pub fn new(
        user: String,
        config_dir: PathBuf,
        default_config_dir: PathBuf,
        oauth_suffix: String,
    ) -> Result<Self, SecureStorageError> {
        if which::which("secret-tool").is_err() {
            return Err(SecureStorageError::BackendUnavailable(
                "libsecret `secret-tool` CLI not found on PATH".into(),
            ));
        }
        Ok(Self {
            user,
            config_dir,
            default_config_dir,
            oauth_suffix,
        })
    }

    /// Convenience constructor with the default `OAUTH_FILE_SUFFIX = ""`.
    ///
    /// # Errors
    /// Same as [`LinuxSecretStorage::new`].
    pub fn new_default_oauth_suffix(
        user: String,
        config_dir: PathBuf,
        default_config_dir: PathBuf,
    ) -> Result<Self, SecureStorageError> {
        Self::new(user, config_dir, default_config_dir, String::new())
    }

    /// Return the full Secret Service service name for the supplied
    /// `service_suffix`.
    ///
    /// Callers pass `"-credentials"` (OAuth) or `""` (legacy API key) — the
    /// "LingXi" prefix and `dir_hash` are interpolated here so the
    /// resulting string matches claude-code's literal layout and the macOS
    /// sibling byte-for-byte.
    pub(crate) fn service_name(&self, service_suffix: &str) -> String {
        let dir_hash = super::helpers::compute_dir_hash(
            self.config_dir.as_path(),
            self.default_config_dir.as_path(),
        );
        full_service_name(
            "LingXi",
            self.oauth_suffix.as_str(),
            service_suffix,
            &dir_hash,
        )
    }
}

#[async_trait]
impl SecureStorage for LinuxSecretStorage {
    async fn store(
        &self,
        service: &str,
        account: &str,
        data: SecureStorageData,
    ) -> Result<(), SecureStorageError> {
        let full_service = self.service_name(service);
        let json = serde_json::to_string(&data)
            .map_err(|e| SecureStorageError::Io(format!("serialize: {e}")))?;

        let (exit_status, stderr) = run_store(&full_service, account, &self.user, &json).await?;
        if !exit_status.success() {
            return Err(SecureStorageError::BackendUnavailable(format!(
                "secret-tool store exited {}: {}",
                exit_status.code().unwrap_or(-1),
                stderr.trim()
            )));
        }
        Ok(())
    }

    async fn retrieve(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<SecureStorageData>, SecureStorageError> {
        let full_service = self.service_name(service);
        run_lookup(&full_service, account, &self.user).await
    }

    async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
        let full_service = self.service_name(service);
        let (exit_status, stderr) = run_clear(&full_service, account, &self.user).await?;
        // `secret-tool clear` on a non-existent item exits 0, so a delete of a
        // missing entry is naturally success — matching the trait contract and
        // the macOS "treat not-found as success" behavior.
        if exit_status.success() {
            return Ok(());
        }
        Err(SecureStorageError::BackendUnavailable(format!(
            "secret-tool clear exited {}: {}",
            exit_status.code().unwrap_or(-1),
            stderr.trim()
        )))
    }

    async fn list(&self, _service: &str) -> Result<Vec<String>, SecureStorageError> {
        Err(SecureStorageError::BackendUnavailable(
            "list not supported on Linux libsecret backend".into(),
        ))
    }

    fn is_encrypted(&self) -> bool {
        true
    }

    fn backend(&self) -> SecureStorageBackend {
        SecureStorageBackend::LinuxLibsecret
    }
}

/// `secret-tool store --label="LingXi" service <svc> account <acct> user <user>`.
///
/// The label is cosmetic; the `(service, account, user)` attribute set is the
/// real lookup key — `user` mirrors the macOS keychain `-a <user>` identity
/// dimension. The JSON value is fed whole to stdin (no argv exposure, no
/// length limit). Returns the exit status and captured stderr so the caller
/// can decorate errors.
async fn run_store(
    full_service: &str,
    account: &str,
    user: &str,
    json: &str,
) -> Result<(std::process::ExitStatus, String), SecureStorageError> {
    let mut child = Command::new("secret-tool")
        .args([
            "store",
            "--label=LingXi",
            "service",
            full_service,
            "account",
            account,
            "user",
            user,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SecureStorageError::Io(format!("spawn secret-tool store: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(json.as_bytes())
            .await
            .map_err(|e| SecureStorageError::Io(format!("stdin write: {e}")))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| SecureStorageError::Io(format!("stdin shutdown: {e}")))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| SecureStorageError::Io(format!("wait: {e}")))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((output.status, stderr))
}

/// `secret-tool lookup service <svc> account <acct> user <user>`.
///
/// On a match `secret-tool` prints the secret to stdout (no trailing newline)
/// and exits 0. On no-match it prints nothing and exits non-zero. The
/// exit/stdout pair is classified by [`classify_lookup`]: a no-match becomes
/// `Ok(None)` (mirroring the macOS `errSecItemNotFound` exit-44 path) while a
/// non-zero exit with diagnostic output (daemon error, locked keyring)
/// surfaces as [`SecureStorageError::BackendUnavailable`].
async fn run_lookup(
    full_service: &str,
    account: &str,
    user: &str,
) -> Result<Option<SecureStorageData>, SecureStorageError> {
    let output = Command::new("secret-tool")
        .args([
            "lookup",
            "service",
            full_service,
            "account",
            account,
            "user",
            user,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SecureStorageError::Io(format!("spawn secret-tool lookup: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout_trimmed = stdout.trim();
    match classify_lookup(output.status.success(), stdout_trimmed) {
        LookupOutcome::NotFound => Ok(None),
        LookupOutcome::Failed => Err(SecureStorageError::BackendUnavailable(format!(
            "secret-tool lookup exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
        LookupOutcome::Found => {
            let data: SecureStorageData = serde_json::from_str(stdout_trimmed).map_err(|e| {
                SecureStorageError::Io(format!("deserialize libsecret payload: {e}"))
            })?;
            Ok(Some(data))
        }
    }
}

/// `secret-tool clear service <svc> account <acct> user <user>`.
///
/// Removes all matching items. Clearing a non-existent item exits 0. Returns
/// the exit status and captured stderr so the caller can decorate errors.
async fn run_clear(
    full_service: &str,
    account: &str,
    user: &str,
) -> Result<(std::process::ExitStatus, String), SecureStorageError> {
    let output = Command::new("secret-tool")
        .args([
            "clear",
            "service",
            full_service,
            "account",
            account,
            "user",
            user,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| SecureStorageError::Io(format!("spawn secret-tool clear: {e}")))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((output.status, stderr))
}

/// Decision for a `secret-tool lookup` result.
#[derive(Debug, PartialEq, Eq)]
enum LookupOutcome {
    /// Exit 0 with a non-empty payload on stdout.
    Found,
    /// Non-zero exit with empty stdout — the "no entry" sentinel.
    NotFound,
    /// Non-zero exit with diagnostic output — a real backend failure.
    Failed,
}

/// Classify a `secret-tool lookup` outcome from its exit status and trimmed
/// stdout.
///
/// `secret-tool lookup` exits non-zero and prints nothing when no item
/// matches; that is the not-found sentinel (analogous to the macOS exit-44
/// `errSecItemNotFound`). A success with payload is [`LookupOutcome::Found`];
/// a non-zero exit that nonetheless produced stdout is treated as a failure so
/// a malformed-but-present payload is not silently swallowed.
fn classify_lookup(status_success: bool, stdout_trimmed: &str) -> LookupOutcome {
    if status_success {
        if stdout_trimmed.is_empty() {
            LookupOutcome::NotFound
        } else {
            LookupOutcome::Found
        }
    } else if stdout_trimmed.is_empty() {
        LookupOutcome::NotFound
    } else {
        LookupOutcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_test_storage(cfg: &str, default: &str) -> LinuxSecretStorage {
        LinuxSecretStorage {
            user: "tester".into(),
            config_dir: PathBuf::from(cfg),
            default_config_dir: PathBuf::from(default),
            oauth_suffix: String::new(),
        }
    }

    #[test]
    fn service_name_default_dir_has_no_dir_hash() {
        let s = mk_test_storage("/home/x/.lingxi", "/home/x/.lingxi");
        assert_eq!(s.service_name("-credentials"), "LingXi-credentials");
        assert_eq!(s.service_name(""), "LingXi");
    }

    #[test]
    fn service_name_non_default_dir_has_dir_hash() {
        let s = mk_test_storage("/home/x/work/.claude-2", "/home/x/.lingxi");
        let svc = s.service_name("-credentials");
        assert!(svc.starts_with("LingXi-credentials-"));
        assert_eq!(svc.len(), "LingXi-credentials-".len() + 8);
    }

    #[test]
    fn classify_lookup_success_with_payload_is_found() {
        assert_eq!(classify_lookup(true, "{\"v\":1}"), LookupOutcome::Found);
    }

    #[test]
    fn classify_lookup_nonzero_empty_is_not_found() {
        assert_eq!(classify_lookup(false, ""), LookupOutcome::NotFound);
    }

    #[test]
    fn classify_lookup_success_empty_is_not_found() {
        assert_eq!(classify_lookup(true, ""), LookupOutcome::NotFound);
    }

    #[test]
    fn classify_lookup_nonzero_with_stderr_payload_is_failed() {
        assert_eq!(
            classify_lookup(false, "daemon error"),
            LookupOutcome::Failed
        );
    }
}
