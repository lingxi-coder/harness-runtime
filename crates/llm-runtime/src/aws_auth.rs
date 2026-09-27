//! Client-side `awsAuthRefresh` / `awsCredentialExport` flow (2.1.198).
//!
//! Ports the real-binary trio around Bedrock-style AWS auth:
//!
//! - `ZBd` — the auto-refresh driver ([`AwsAuthRefresher::refresh`]): resolve
//!   the `awsAuthRefresh` command from settings, refuse project/local-sourced
//!   commands before workspace trust (telemetry
//!   `tengu_awsAuthRefresh_missing_trust`), memoize the in-flight run, skip
//!   when an STS caller-identity probe succeeds (credentials are actually
//!   valid), honour a 30-second cooldown between attempts (`QBd = 30000`),
//!   then run the script with a 3-minute timeout (`e2d = 180000`).
//! - `gIn` — the script runner (subprocess seam [`AwsAuthProcess`]): exit 0 ⇒
//!   success; SIGTERM-by-timeout ⇒ the byte-locked 3-minute message; any other
//!   failure ⇒ the `Error running awsAuthRefresh …` message.
//! - `t2d` — [`AwsAuthRefresher::export_credentials`]: run the
//!   `awsCredentialExport` script (same trust gate, telemetry
//!   `tengu_awsCredentialExport_missing_trust`) and parse its stdout as STS
//!   JSON (`wdi`/`Rdi` validation).
//!
//! The drive-loop trigger ([`is_aws_auth_error`], binary `V_c`/`G_c`/`s_f`) is
//! PROVIDER-GATED: only [`ProviderId::BedrockClaude`] — LingXi's analogue of
//! the binary's Bedrock / anthropicAws / Mantle AWS-auth providers — ever
//! reaches the refresh path. OpenAI/GLM/Gemini/… are never touched.
//!
//! Everything effectful goes through the [`AwsAuthProcess`] trait seam
//! (mirrors the sandbox-runner injection idiom) so tests inject a fixture.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::Shared;
use futures::FutureExt;

use crate::{BoxFuture, LlmError, ProviderId};

// ── Byte-locked constants (2.1.198) ─────────────────────────────────────────

/// `QBd = 30000` — cooldown between refresh attempts (verified in the binary's
/// var block: `…XBd=60000,QBd=30000,mIn=null,dqe=null,Ozr=0,e2d=180000…`).
pub const AWS_AUTH_REFRESH_COOLDOWN: Duration = Duration::from_millis(30_000);

/// `e2d = 180000` — the refresh script's 3-minute exec timeout (`gIn` passes
/// `{timeout: e2d}` to `exec`).
pub const AWS_AUTH_REFRESH_TIMEOUT: Duration = Duration::from_millis(180_000);

/// `Ygf = 2` — per-request bound on AWS-auth-triggered retries. The binary's
/// retry loop counts `V_c`-classified errors and throws
/// `api_request_aws_auth_exhausted` once `u >= Ygf`.
pub const AWS_AUTH_MAX_ATTEMPTS: u32 = 2;

/// `gIn` SIGTERM branch (timeout).
pub const AWS_AUTH_REFRESH_TIMEOUT_MESSAGE: &str =
    "AWS auth refresh timed out after 3 minutes. Run your auth command manually in a separate terminal.";

/// `gIn` non-timeout failure branch.
pub const AWS_AUTH_REFRESH_ERROR_PREFIX: &str =
    "Error running awsAuthRefresh (in settings or ~/.claude.json):";

/// Login-flow success copy (`aws_refresh_done` screen).
pub const AWS_AUTH_REFRESH_SUCCESS_MESSAGE: &str = "AWS credentials refreshed.";

/// Login-flow failure copy (`aws_refresh_done` screen).
pub const AWS_AUTH_REFRESH_FAILURE_MESSAGE: &str =
    "awsAuthRefresh failed. Check the command in your settings and try running it in a separate terminal.";

/// `t2d` catch branch.
pub const AWS_CREDENTIAL_EXPORT_ERROR_PREFIX: &str =
    "Error getting AWS credentials from awsCredentialExport (in settings or ~/.claude.json):";

// ── Settings snapshot ────────────────────────────────────────────────────────

/// Snapshot of the AWS-auth settings the flow consumes.
///
/// The host resolves these once from the merged settings stack:
/// - `*_from_project` mirrors the binary's `mqe`/`Gzr` — `true` when the
///   effective command value came from `.claude/settings.json` (project) or
///   `.claude/settings.local.json` (local), the layers an untrusted repo
///   controls.
/// - `workspace_trusted` mirrors `yd() || hr()` — workspace trust confirmed.
#[derive(Debug, Clone, Default)]
pub struct AwsAuthSettings {
    /// `awsAuthRefresh` — script that refreshes AWS authentication.
    pub aws_auth_refresh: Option<String>,
    /// `true` when `aws_auth_refresh` is project/local-settings-sourced.
    pub aws_auth_refresh_from_project: bool,
    /// `awsCredentialExport` — script that exports AWS credentials.
    pub aws_credential_export: Option<String>,
    /// `true` when `aws_credential_export` is project/local-settings-sourced.
    pub aws_credential_export_from_project: bool,
    /// Workspace trust confirmed (`yd() || hr()`).
    pub workspace_trusted: bool,
}

// ── Subprocess / STS seam ────────────────────────────────────────────────────

/// Terminal state of one refresh-script run (the `gIn` close handler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshRunOutcome {
    /// Exit code 0.
    Success,
    /// Killed by the 3-minute timeout (`i === "SIGTERM"` and not aborted).
    TimedOut,
    /// Any other non-zero exit / spawn failure.
    Failed,
}

/// Effect seam for the AWS auth flow: STS probe + script execution.
///
/// Prod impl is [`ShellAwsAuthProcess`]; tests inject a fixture (same idiom as
/// the sandbox-runner injection).
pub trait AwsAuthProcess: Send + Sync + fmt::Debug {
    /// `xdi()`: STS `GetCallerIdentity` probe. `Ok(())` means the ambient AWS
    /// credentials are currently valid (the refresh is skipped).
    fn caller_identity(&self) -> BoxFuture<'_, Result<(), String>>;

    /// `gIn` exec: run the refresh command with `timeout`.
    fn run_refresh<'a>(
        &'a self,
        command: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, RefreshRunOutcome>;

    /// `hx` exec for `awsCredentialExport`: run the command and capture
    /// `(exit_code, stdout)`. `Err` = spawn failure.
    fn run_export<'a>(&'a self, command: &'a str) -> BoxFuture<'a, Result<(i32, String), String>>;
}

/// Production [`AwsAuthProcess`]: shells out via `sh -c` (the binary's
/// `child_process.exec` also runs through a shell).
///
/// APPROXIMATION NOTE: the binary probes STS in-process through the JS AWS SDK
/// (`STSClient` + `GetCallerIdentityCommand` over the ambient credential
/// chain). LingXi carries no AWS SDK, so the probe shells out to
/// `aws sts get-caller-identity`. A missing `aws` CLI makes the probe fail,
/// which — exactly like the binary's `catch` — proceeds to the refresh script
/// (fail-open toward refreshing, never toward skipping).
#[derive(Debug, Default)]
pub struct ShellAwsAuthProcess;

impl ShellAwsAuthProcess {
    /// STS probe command line.
    const CALLER_IDENTITY_CMD: &'static str = "aws sts get-caller-identity";

    async fn run_shell(command: &str) -> Result<std::process::Output, String> {
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| e.to_string())
    }
}

impl AwsAuthProcess for ShellAwsAuthProcess {
    fn caller_identity(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async {
            let out = Self::run_shell(Self::CALLER_IDENTITY_CMD).await?;
            if out.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
            }
        })
    }

    fn run_refresh<'a>(
        &'a self,
        command: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, RefreshRunOutcome> {
        Box::pin(async move {
            let mut child = match tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(_) => return RefreshRunOutcome::Failed,
            };
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(Ok(status)) if status.success() => RefreshRunOutcome::Success,
                Ok(_) => RefreshRunOutcome::Failed,
                Err(_elapsed) => {
                    // exec's `{timeout}` SIGTERMs the child; mirror that.
                    let _ = child.kill().await;
                    RefreshRunOutcome::TimedOut
                }
            }
        })
    }

    fn run_export<'a>(&'a self, command: &'a str) -> BoxFuture<'a, Result<(i32, String), String>> {
        Box::pin(async move {
            let out = Self::run_shell(command).await?;
            Ok((
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).to_string(),
            ))
        })
    }
}

// ── STS output validation (`Rdi` / `wdi`) ───────────────────────────────────

/// Credentials produced by `awsCredentialExport` (`t2d` return shape).
#[derive(Clone, PartialEq, Eq)]
pub struct AwsExportedCredentials {
    /// `AccessKeyId`.
    pub access_key_id: String,
    /// `SecretAccessKey`.
    pub secret_access_key: String,
    /// `SessionToken`.
    pub session_token: String,
    /// `Expiration` parsed to epoch-milliseconds (`Date.parse`); `None` when
    /// absent or unparseable (`Number.isFinite` gate).
    pub expiration_ms: Option<i64>,
}

impl fmt::Debug for AwsExportedCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsExportedCredentials")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field("session_token", &"[REDACTED]")
            .field("expiration_ms", &self.expiration_ms)
            .finish()
    }
}

/// `Rdi(e)`: the three key fields are non-empty strings.
fn is_sts_credentials_shape(v: &serde_json::Value) -> bool {
    let non_empty_str = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    v.is_object()
        && non_empty_str("AccessKeyId")
        && non_empty_str("SecretAccessKey")
        && non_empty_str("SessionToken")
}

/// `wdi(e)`: accept `{Credentials: {...}}` (nested STS output) or the flat
/// credentials object; anything else is `None`.
#[must_use]
pub fn parse_sts_output(v: &serde_json::Value) -> Option<AwsExportedCredentials> {
    let creds = match v.get("Credentials") {
        Some(nested) if is_sts_credentials_shape(nested) => nested,
        _ if is_sts_credentials_shape(v) => v,
        _ => return None,
    };
    let s = |k: &str| {
        creds
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    // `Date.parse(o)` → ms; `Number.isFinite(s) ? s : void 0`.
    let expiration_ms = creds
        .get("Expiration")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| {
            chrono::DateTime::parse_from_rfc3339(raw)
                .ok()
                .map(|dt| dt.timestamp_millis())
        });
    Some(AwsExportedCredentials {
        access_key_id: s("AccessKeyId"),
        secret_access_key: s("SecretAccessKey"),
        session_token: s("SessionToken"),
        expiration_ms,
    })
}

// ── Drive-loop trigger classification (`V_c` / `G_c`) ───────────────────────

/// Should this terminal API error trigger the AWS auth-refresh path?
///
/// Binary 2.1.198:
/// - `V_c(e, model)`: `e` is an APIError with status **401** AND the model's
///   provider is `anthropicAws` or `mantle`.
/// - `G_c(e, model)`: when `CLAUDE_CODE_USE_BEDROCK` / `_USE_ANTHROPIC_AWS` /
///   `_USE_MANTLE` is env-truthy — `CredentialsProviderError` or status
///   **403** also qualifies; otherwise falls through to `V_c`.
/// - `s_f`: on a match, clears the memoized AWS credential resolver (`xce()`)
///   so the retry re-resolves — which runs `ZBd` (awsAuthRefresh) + `t2d`.
///
/// LingXi routes provider identity explicitly per request
/// ([`ResolvedRoute::provider_id`](crate::ResolvedRoute)) instead of global
/// env flags, so [`ProviderId::BedrockClaude`] — the only AWS-SigV4 provider
/// LingXi models — gates BOTH branches: 401 ⇒ [`LlmError::Authentication`],
/// 403 ⇒ [`LlmError::PermissionDenied`]. Every non-AWS provider returns
/// `false` unconditionally (multi-provider structure is sacrosanct).
#[must_use]
pub fn is_aws_auth_error(error: &LlmError, provider_id: &ProviderId) -> bool {
    if !matches!(provider_id, ProviderId::BedrockClaude) {
        return false;
    }
    matches!(
        error,
        LlmError::Authentication { .. } | LlmError::PermissionDenied { .. }
    )
}

// ── Refresh driver (`ZBd` / `t2d`) ──────────────────────────────────────────

/// Object-safe refresh seam the drive loops call — lets service tests inject
/// a counting fixture without standing up subprocess machinery.
pub trait AwsAuthRefresh: Send + Sync + fmt::Debug {
    /// `ZBd()`: run the auto-refresh flow. Resolves `true` iff a refresh
    /// command ran to successful completion.
    fn refresh(&self) -> BoxFuture<'_, bool>;
}

type SharedRefresh = Shared<BoxFuture<'static, bool>>;

/// Production `ZBd`/`t2d` driver.
///
/// Holds the module-level state the binary keeps in closure vars:
/// `dqe` (in-flight memo) → [`Inner::inflight`], `mIn` (last-attempt
/// timestamp) → [`Inner::last_attempt`], `Ozr` (generation counter, bumped by
/// `tat()` on settings change) → [`Inner::generation`].
#[derive(Debug, Clone)]
pub struct AwsAuthRefresher {
    inner: Arc<Inner>,
}

struct Inner {
    settings: Mutex<AwsAuthSettings>,
    process: Arc<dyn AwsAuthProcess>,
    analytics: Option<Arc<telemetry::AnalyticsBus>>,
    /// `dqe`: the in-flight refresh, shared so concurrent callers await ONE run.
    inflight: Mutex<Option<SharedRefresh>>,
    /// `mIn`: completion time of the last refresh attempt (cooldown anchor).
    last_attempt: Mutex<Option<Instant>>,
    /// `Ozr`: generation counter; a run started under an older generation must
    /// not re-arm the cooldown (`if (t === Ozr) mIn = Date.now()`).
    generation: AtomicU64,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsAuthRefresher")
            .field("process", &self.process)
            .finish_non_exhaustive()
    }
}

impl AwsAuthRefresher {
    /// Build the driver from a settings snapshot, a subprocess seam, and an
    /// optional analytics bus (for the two trust-gate events).
    #[must_use]
    pub fn new(
        settings: AwsAuthSettings,
        process: Arc<dyn AwsAuthProcess>,
        analytics: Option<Arc<telemetry::AnalyticsBus>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                settings: Mutex::new(settings),
                process,
                analytics,
                inflight: Mutex::new(None),
                last_attempt: Mutex::new(None),
                generation: AtomicU64::new(0),
            }),
        }
    }

    /// `tat()`: reset the cooldown and bump the generation. The binary calls
    /// this on settings change and after an interactive `awsAuthRefresh` run.
    pub fn reset(&self) {
        *self
            .inner
            .last_attempt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Replace the settings snapshot (host-side settings reload).
    pub fn set_settings(&self, settings: AwsAuthSettings) {
        *self
            .inner
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings;
    }

    /// `t2d()`: run `awsCredentialExport` and parse its stdout as STS JSON.
    ///
    /// Returns `None` when no command is configured, when the trust gate
    /// refuses a project/local-sourced command (telemetry
    /// `tengu_awsCredentialExport_missing_trust`), or on any run/parse error
    /// (logged with the byte-locked `Error getting AWS credentials …` prefix).
    pub async fn export_credentials(&self) -> Option<AwsExportedCredentials> {
        let inner = &self.inner;
        let (cmd, from_project, trusted) = {
            let s = inner
                .settings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                s.aws_credential_export.clone(),
                s.aws_credential_export_from_project,
                s.workspace_trusted,
            )
        };
        let cmd = cmd?;
        // `Gzr` + `!yd() && !hr()` trust gate.
        if from_project && !trusted {
            tracing::error!(
                "Security: awsCredentialExport executed before workspace trust is confirmed."
            );
            emit_empty(
                &inner.analytics,
                telemetry::tengu::oauth::AWS_CREDENTIAL_EXPORT_MISSING_TRUST,
            )
            .await;
            return None;
        }
        tracing::debug!("Running AWS credential export command");
        let parsed = match inner.process.run_export(&cmd).await {
            Ok((0, stdout)) if !stdout.trim().is_empty() => {
                serde_json::from_str::<serde_json::Value>(stdout.trim())
                    .ok()
                    .as_ref()
                    .and_then(parse_sts_output)
                    .ok_or_else(|| {
                        "awsCredentialExport did not return valid AWS STS output structure"
                            .to_string()
                    })
            }
            Ok(_) => Err("awsCredentialExport did not return a valid value".to_string()),
            Err(e) => Err(e),
        };
        match parsed {
            Ok(creds) => {
                tracing::debug!("AWS credentials retrieved from awsCredentialExport");
                Some(creds)
            }
            Err(msg) => {
                tracing::error!("{AWS_CREDENTIAL_EXPORT_ERROR_PREFIX} {msg}");
                None
            }
        }
    }
}

impl AwsAuthRefresh for AwsAuthRefresher {
    /// `ZBd()` — see the module docs for the exact sequence.
    fn refresh(&self) -> BoxFuture<'_, bool> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let (cmd, from_project, trusted) = {
                let s = inner
                    .settings
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    s.aws_auth_refresh.clone(),
                    s.aws_auth_refresh_from_project,
                    s.workspace_trusted,
                )
            };
            // `let t = Ozr` — snapshot the generation before any await.
            let generation = inner.generation.load(Ordering::SeqCst);
            // `if (!e) return false`.
            let Some(cmd) = cmd else { return false };
            // `mqe()` + `!yd() && !hr()` — project/local-sourced command before
            // trust confirmation is refused.
            if from_project && !trusted {
                tracing::error!(
                    "Security: awsAuthRefresh executed before workspace trust is confirmed."
                );
                emit_empty(
                    &inner.analytics,
                    telemetry::tengu::oauth::AWS_AUTH_REFRESH_MISSING_TRUST,
                )
                .await;
                return false;
            }
            // `if (dqe) return dqe` — join the in-flight run. (Guard scoped so
            // the lock is never held across the await.)
            let existing = {
                inner
                    .inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            };
            if let Some(shared) = existing {
                return shared.await;
            }
            // STS caller-identity probe: success ⇒ credentials are valid, skip.
            tracing::debug!("Fetching AWS caller identity for AWS auth refresh command");
            if inner.process.caller_identity().await.is_ok() {
                tracing::debug!("Fetched AWS caller identity, skipping AWS auth refresh command");
                return false;
            }
            // Probe failed (the binary's `catch`): re-check `dqe` — the await
            // above may have interleaved with another caller — else create the
            // run under the lock so exactly one run exists.
            let shared = {
                let mut guard = inner
                    .inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(existing) = guard.clone() {
                    existing
                } else {
                    // `mIn !== null && Date.now() - mIn < QBd` — cooldown.
                    let within_cooldown = inner
                        .last_attempt
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some_and(|at| at.elapsed() < AWS_AUTH_REFRESH_COOLDOWN);
                    if within_cooldown {
                        return false;
                    }
                    let run_inner = Arc::clone(&inner);
                    let fut: BoxFuture<'static, bool> = Box::pin(async move {
                        let ok = run_refresh_command(&*run_inner.process, &cmd).await;
                        // `finally { if (t === Ozr) mIn = Date.now(); dqe = null }`.
                        if run_inner.generation.load(Ordering::SeqCst) == generation {
                            *run_inner
                                .last_attempt
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(Instant::now());
                        }
                        *run_inner
                            .inflight
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                        ok
                    });
                    let shared = fut.shared();
                    *guard = Some(shared.clone());
                    shared
                }
            };
            shared.await
        })
    }
}

/// `gIn(e)` outcome handling: run the script and surface the byte-locked copy.
async fn run_refresh_command(process: &dyn AwsAuthProcess, command: &str) -> bool {
    tracing::debug!("Running AWS auth refresh command");
    match process.run_refresh(command, AWS_AUTH_REFRESH_TIMEOUT).await {
        RefreshRunOutcome::Success => {
            tracing::debug!("AWS auth refresh completed successfully");
            tracing::info!("{AWS_AUTH_REFRESH_SUCCESS_MESSAGE}");
            true
        }
        RefreshRunOutcome::TimedOut => {
            tracing::error!("{AWS_AUTH_REFRESH_TIMEOUT_MESSAGE}");
            false
        }
        RefreshRunOutcome::Failed => {
            tracing::error!("{AWS_AUTH_REFRESH_ERROR_PREFIX}");
            tracing::error!("{AWS_AUTH_REFRESH_FAILURE_MESSAGE}");
            false
        }
    }
}

/// Emit a `tengu_*` event with the binary's empty `{}` payload.
async fn emit_empty(bus: &Option<Arc<telemetry::AnalyticsBus>>, name: &'static str) {
    if let Some(bus) = bus {
        bus.log_event(name, telemetry::LogEventMetadata::new())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Scriptable fixture for the subprocess seam.
    #[derive(Debug)]
    struct FixtureProcess {
        /// `Ok(())` ⇒ probe succeeds (refresh skipped).
        caller_identity_ok: bool,
        refresh_outcome: RefreshRunOutcome,
        refresh_calls: AtomicU32,
        probe_calls: AtomicU32,
        /// Optional gate: refresh future waits until released (concurrency test).
        hold: Option<Arc<tokio::sync::Notify>>,
    }

    impl FixtureProcess {
        fn new(caller_identity_ok: bool, refresh_outcome: RefreshRunOutcome) -> Self {
            Self {
                caller_identity_ok,
                refresh_outcome,
                refresh_calls: AtomicU32::new(0),
                probe_calls: AtomicU32::new(0),
                hold: None,
            }
        }
    }

    impl AwsAuthProcess for FixtureProcess {
        fn caller_identity(&self) -> BoxFuture<'_, Result<(), String>> {
            self.probe_calls.fetch_add(1, Ordering::SeqCst);
            let ok = self.caller_identity_ok;
            Box::pin(async move {
                if ok {
                    Ok(())
                } else {
                    Err("ExpiredToken".to_string())
                }
            })
        }

        fn run_refresh<'a>(
            &'a self,
            _command: &'a str,
            _timeout: Duration,
        ) -> BoxFuture<'a, RefreshRunOutcome> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self.refresh_outcome;
            let hold = self.hold.clone();
            Box::pin(async move {
                if let Some(gate) = hold {
                    gate.notified().await;
                }
                outcome
            })
        }

        fn run_export<'a>(
            &'a self,
            _command: &'a str,
        ) -> BoxFuture<'a, Result<(i32, String), String>> {
            Box::pin(async { Err("not scripted".to_string()) })
        }
    }

    fn settings(cmd: Option<&str>, from_project: bool, trusted: bool) -> AwsAuthSettings {
        AwsAuthSettings {
            aws_auth_refresh: cmd.map(str::to_string),
            aws_auth_refresh_from_project: from_project,
            aws_credential_export: None,
            aws_credential_export_from_project: false,
            workspace_trusted: trusted,
        }
    }

    // ── ZBd sequence ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn no_command_is_a_no_op() {
        let process = Arc::new(FixtureProcess::new(false, RefreshRunOutcome::Success));
        let r = AwsAuthRefresher::new(settings(None, false, true), process.clone(), None);
        assert!(!r.refresh().await);
        assert_eq!(
            process.probe_calls.load(Ordering::SeqCst),
            0,
            "no probe without a command"
        );
        assert_eq!(process.refresh_calls.load(Ordering::SeqCst), 0);
    }

    /// Build a bus with a capturing sink attached.
    async fn bus_with_sink() -> (Arc<telemetry::AnalyticsBus>, Arc<telemetry::InMemorySink>) {
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        let sink = Arc::new(telemetry::InMemorySink::new());
        bus.attach_sink(sink.clone()).await;
        (bus, sink)
    }

    #[tokio::test]
    async fn project_sourced_command_without_trust_is_refused() {
        // ZBd: mqe() && !yd() && !hr() ⇒ error + tengu_awsAuthRefresh_missing_trust + false.
        let process = Arc::new(FixtureProcess::new(false, RefreshRunOutcome::Success));
        let (bus, sink) = bus_with_sink().await;
        let r = AwsAuthRefresher::new(
            settings(Some("./refresh.sh"), true, false),
            process.clone(),
            Some(bus),
        );
        assert!(!r.refresh().await);
        assert_eq!(
            process.refresh_calls.load(Ordering::SeqCst),
            0,
            "script must NOT run"
        );
        assert_eq!(
            process.probe_calls.load(Ordering::SeqCst),
            0,
            "trust gate precedes the probe"
        );
        let events = sink.events().await;
        assert_eq!(events.len(), 1, "exactly one telemetry event");
        assert_eq!(events[0].name, "tengu_awsAuthRefresh_missing_trust");
        assert!(
            events[0].metadata.is_empty(),
            "binary emits an empty payload"
        );
    }

    #[tokio::test]
    async fn project_sourced_command_with_trust_runs() {
        let process = Arc::new(FixtureProcess::new(false, RefreshRunOutcome::Success));
        let r = AwsAuthRefresher::new(
            settings(Some("./refresh.sh"), true, true),
            process.clone(),
            None,
        );
        assert!(r.refresh().await);
        assert_eq!(process.refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn valid_caller_identity_skips_refresh() {
        // STS probe succeeds ⇒ "skipping AWS auth refresh command" ⇒ false.
        let process = Arc::new(FixtureProcess::new(true, RefreshRunOutcome::Success));
        let r = AwsAuthRefresher::new(
            settings(Some("./refresh.sh"), false, true),
            process.clone(),
            None,
        );
        assert!(!r.refresh().await);
        assert_eq!(process.probe_calls.load(Ordering::SeqCst), 1);
        assert_eq!(process.refresh_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn success_path_returns_true_and_arms_cooldown() {
        let process = Arc::new(FixtureProcess::new(false, RefreshRunOutcome::Success));
        let r = AwsAuthRefresher::new(
            settings(Some("./refresh.sh"), false, true),
            process.clone(),
            None,
        );
        assert!(r.refresh().await, "exit 0 resolves true");
        assert_eq!(process.refresh_calls.load(Ordering::SeqCst), 1);
        // Second attempt inside QBd=30s ⇒ cooldown short-circuits to false.
        assert!(
            !r.refresh().await,
            "cooldown (QBd) suppresses the second run"
        );
        assert_eq!(
            process.refresh_calls.load(Ordering::SeqCst),
            1,
            "script ran once"
        );
        // tat() resets the cooldown ⇒ the script may run again.
        r.reset();
        assert!(r.refresh().await);
        assert_eq!(process.refresh_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn timeout_resolves_false() {
        // gIn SIGTERM branch → "AWS auth refresh timed out after 3 minutes…" → false.
        let process = Arc::new(FixtureProcess::new(false, RefreshRunOutcome::TimedOut));
        let r = AwsAuthRefresher::new(
            settings(Some("./slow.sh"), false, true),
            process.clone(),
            None,
        );
        assert!(!r.refresh().await);
        assert_eq!(process.refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failure_resolves_false() {
        let process = Arc::new(FixtureProcess::new(false, RefreshRunOutcome::Failed));
        let r = AwsAuthRefresher::new(settings(Some("./bad.sh"), false, true), process, None);
        assert!(!r.refresh().await);
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_inflight_run() {
        // dqe memoization: two concurrent refresh() calls ⇒ ONE script run,
        // both callers observe its result.
        let gate = Arc::new(tokio::sync::Notify::new());
        let mut process = FixtureProcess::new(false, RefreshRunOutcome::Success);
        process.hold = Some(gate.clone());
        let process = Arc::new(process);
        let r = AwsAuthRefresher::new(
            settings(Some("./refresh.sh"), false, true),
            process.clone(),
            None,
        );

        let r1 = r.clone();
        let t1 = tokio::spawn(async move { r1.refresh().await });
        let r2 = r.clone();
        let t2 = tokio::spawn(async move { r2.refresh().await });
        // Let both tasks reach the gate, then release the (single) run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.notify_waiters();
        gate.notify_one();
        let (a, b) = (t1.await.unwrap(), t2.await.unwrap());
        assert!(a && b, "both callers observe the shared success");
        assert_eq!(
            process.refresh_calls.load(Ordering::SeqCst),
            1,
            "exactly one run"
        );
    }

    // ── t2d / wdi ────────────────────────────────────────────────────────────

    #[test]
    fn parse_sts_output_accepts_nested_credentials() {
        let v: serde_json::Value = serde_json::json!({
            "Credentials": {
                "AccessKeyId": "AKIA123",
                "SecretAccessKey": "secret",
                "SessionToken": "token",
                "Expiration": "2026-07-02T12:00:00+00:00"
            }
        });
        let c = parse_sts_output(&v).expect("nested Credentials accepted");
        assert_eq!(c.access_key_id, "AKIA123");
        assert_eq!(
            c.expiration_ms,
            Some(1_782_993_600_000),
            "2026-07-02T12:00:00Z in epoch ms"
        );
    }

    #[test]
    fn parse_sts_output_accepts_flat_shape_and_tolerates_bad_expiration() {
        let v: serde_json::Value = serde_json::json!({
            "AccessKeyId": "AKIA123",
            "SecretAccessKey": "secret",
            "SessionToken": "token",
            "Expiration": "not-a-date"
        });
        let c = parse_sts_output(&v).expect("flat shape accepted");
        assert_eq!(
            c.expiration_ms, None,
            "unparseable Expiration → None (Number.isFinite gate)"
        );
    }

    #[test]
    fn parse_sts_output_rejects_empty_or_missing_fields() {
        // Rdi: three non-empty strings required.
        for bad in [
            serde_json::json!({"AccessKeyId": "", "SecretAccessKey": "s", "SessionToken": "t"}),
            serde_json::json!({"AccessKeyId": "a", "SecretAccessKey": "s"}),
            serde_json::json!("not an object"),
            serde_json::json!(null),
        ] {
            assert!(parse_sts_output(&bad).is_none(), "must reject: {bad}");
        }
    }

    #[tokio::test]
    async fn export_trust_gate_emits_its_own_event() {
        let process = Arc::new(FixtureProcess::new(false, RefreshRunOutcome::Success));
        let (bus, sink) = bus_with_sink().await;
        let r = AwsAuthRefresher::new(
            AwsAuthSettings {
                aws_credential_export: Some("./export.sh".to_string()),
                aws_credential_export_from_project: true,
                workspace_trusted: false,
                ..AwsAuthSettings::default()
            },
            process,
            Some(bus),
        );
        assert!(r.export_credentials().await.is_none());
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "tengu_awsCredentialExport_missing_trust");
    }

    // ── V_c / G_c classification ─────────────────────────────────────────────

    #[test]
    fn aws_auth_error_is_provider_gated() {
        // 401/403 on the Bedrock provider trigger; every other provider never does.
        assert!(is_aws_auth_error(
            &LlmError::Authentication {
                message: String::new()
            },
            &ProviderId::BedrockClaude
        ));
        assert!(is_aws_auth_error(
            &LlmError::PermissionDenied {
                message: String::new()
            },
            &ProviderId::BedrockClaude
        ));
        assert!(!is_aws_auth_error(
            &LlmError::ProviderInternal,
            &ProviderId::BedrockClaude
        ));
        for provider in [
            ProviderId::AnthropicFirstParty,
            ProviderId::OpenAI,
            ProviderId::Gemini,
            ProviderId::VertexClaude,
            ProviderId::AzureOpenAI,
            ProviderId::OpenAICompatible {
                name: "glm".to_string(),
            },
            ProviderId::Custom {
                name: "x".to_string(),
            },
        ] {
            assert!(
                !is_aws_auth_error(
                    &LlmError::Authentication {
                        message: String::new()
                    },
                    &provider
                ),
                "refresh must never trigger for {provider:?}"
            );
        }
    }

    #[test]
    fn constants_match_2_1_198_binary() {
        // QBd=30000, e2d=180000, Ygf=2 (binary var block near ZBd).
        assert_eq!(AWS_AUTH_REFRESH_COOLDOWN, Duration::from_millis(30_000));
        assert_eq!(AWS_AUTH_REFRESH_TIMEOUT, Duration::from_millis(180_000));
        assert_eq!(AWS_AUTH_MAX_ATTEMPTS, 2);
    }
}
