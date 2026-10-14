//! Fixed-Bun JavaScript policy semantics and Native policy-warning formatting.
//! Core owns the typed carriers; this module owns the exact Bun execution and
//! JSONL process boundary for those source-derived operations.

use lingxi_core::settings::native_policy_js_semantics::{
    NativePolicyDisplayRow, NativePolicyJsSemanticsRegistry, NativePolicyUrlSemanticRow,
    NativePolicyZodUrlOutcome,
};
use lingxi_core::settings::native_policy_regex::{
    NativePolicyRegexValidationOutcome, NativePolicyRegexValidationRow,
    NativePolicyRegexWorkerError,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

const EXPECTED_BUN_VERSION: &str = "1.4.3";
const EXPECTED_BUN_REVISION: &str = "c6da4a4d3010e5553438c60f6bd76d981976867c";
const EXPECTED_BUN_FINGERPRINT: &str = "1.4.3@c6da4a4d3010e5553438c60f6bd76d981976867c";
const PROTOCOL_VERSION: u64 = 4;
const WORKER_ID: &str = "native-policy-js-semantics-2.1.295-v4";
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(60);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_BUNDLE_ID: AtomicU64 = AtomicU64::new(1);

const WORKER_ASSET: &[u8] = include_bytes!("../assets/bun_policy_semantics_worker.mjs");

type WorkerResult<T> = Result<T, NativePolicyRegexWorkerError>;

type Reply = Value;

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

#[derive(Default)]
struct Session {
    process: Option<Process>,
    bundle: Option<PrivateBundle>,
    poisoned: bool,
    verified_fingerprint: Option<String>,
    next_request_id: u64,
}

/// One persistent Bun process, serialized across all requests. `preflight`
/// must succeed before startup validation consumes any RegExp results.
pub struct BunPolicyJsSemanticsWorker {
    executable: PathBuf,
    session: Mutex<Session>,
}

struct ExchangePoisonGuard<'a> {
    session: &'a mut Session,
    complete: bool,
}

impl<'a> ExchangePoisonGuard<'a> {
    fn new(session: &'a mut Session) -> Self {
        Self {
            session,
            complete: false,
        }
    }

    fn complete(&mut self) {
        self.complete = true;
    }
}

impl Drop for ExchangePoisonGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.session.poisoned = true;
            self.session.verified_fingerprint = None;
        }
    }
}

impl BunPolicyJsSemanticsWorker {
    /// Resolve the exact host-configured Bun executable used by ModHost. An
    /// explicit executable wins; development falls back to the same `bun`
    /// command lookup as Insights. The worker itself requires exactly 1.4.3.
    pub fn new_from_host_environment() -> Result<Self, NativePolicyRegexWorkerError> {
        let executable = resolve_bun_executable()?;
        Ok(Self {
            executable,
            session: Mutex::new(Session {
                next_request_id: 1,
                ..Session::default()
            }),
        })
    }

    #[cfg(test)]
    fn with_executable(executable: PathBuf) -> Self {
        Self {
            executable,
            session: Mutex::new(Session {
                next_request_id: 1,
                ..Session::default()
            }),
        }
    }

    /// Starts the worker and checks its runtime/version/protocol before any
    /// validation result can be consumed. Repeated calls reuse the same child.
    #[cfg(test)]
    pub async fn preflight(&self) -> Result<String, NativePolicyRegexWorkerError> {
        let mut session = self.session.lock().await;
        self.ensure_preflight(&mut session).await
    }

    /// Validate unique patterns in first-seen order. Keys are exact UTF-16
    /// code-unit vectors, matching JavaScript string identity even for lone
    /// surrogates. Worker/process failures remain outer errors; a constructor
    /// rejection is a typed per-pattern `Invalid` result.
    pub async fn validate_patterns(
        &self,
        patterns: &[Vec<u16>],
    ) -> Result<Vec<NativePolicyRegexValidationRow>, NativePolicyRegexWorkerError> {
        let mut session = self.session.lock().await;
        self.ensure_preflight(&mut session).await?;

        let patterns = unique_patterns(patterns);
        if patterns.is_empty() {
            return Ok(Vec::new());
        }

        let reply = self
            .exchange_locked(
                &mut session,
                "validate_patterns",
                json!({ "patterns": patterns.clone() }),
                EXCHANGE_TIMEOUT,
            )
            .await?;
        match parse_validation_reply(&reply, &patterns) {
            Ok(rows) => Ok(rows),
            Err(error) => {
                session.poisoned = true;
                session.verified_fingerprint = None;
                let _ = self.reap_poisoned(&mut session).await;
                Err(error)
            }
        }
    }

    /// Observe exact JavaScript URL and df display semantics for unique UTF-16
    /// inputs in first-seen order. URL-invalid is a typed row outcome; worker,
    /// protocol, or capability failures remain outer errors.
    pub async fn observe_semantics(
        &self,
        url_inputs: &[Vec<u16>],
        display_inputs: &[Vec<u16>],
    ) -> Result<NativePolicyJsSemanticsRegistry, NativePolicyRegexWorkerError> {
        let mut session = self.session.lock().await;
        self.ensure_preflight(&mut session).await?;

        let url_inputs = unique_patterns(url_inputs);
        let display_inputs = unique_patterns(display_inputs);
        if url_inputs.is_empty() && display_inputs.is_empty() {
            return NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new())
                .map_err(|_| protocol_error("empty semantic result was rejected"));
        }

        let reply = self
            .exchange_locked(
                &mut session,
                "observe_semantics",
                json!({
                    "url_inputs": url_inputs.clone(),
                    "display_inputs": display_inputs.clone(),
                }),
                EXCHANGE_TIMEOUT,
            )
            .await?;
        match parse_semantics_reply(&reply, &url_inputs, &display_inputs) {
            Ok(registry) => Ok(registry),
            Err(error) => {
                session.poisoned = true;
                session.verified_fingerprint = None;
                let _ = self.reap_poisoned(&mut session).await;
                Err(error)
            }
        }
    }

    /// Apply Native's debug filter, D.redact, invisible/control escaping, and
    /// log-line framing to source-exact UTF-16 warning messages. A `None` row
    /// means the Native debug filter rejected that message. Duplicate inputs
    /// and their order are preserved because Native emits each warning.
    pub async fn render_policy_warnings(
        &self,
        messages: &[Vec<u16>],
        filter_source_units: Option<&[u16]>,
    ) -> Result<Vec<Option<Vec<u16>>>, NativePolicyRegexWorkerError> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        let mut session = self.session.lock().await;
        self.ensure_preflight(&mut session).await?;
        let filter_units = filter_source_units.map(|units| units.to_vec());
        let reply = self
            .exchange_locked(
                &mut session,
                "render_policy_warnings",
                json!({
                    "messages": messages,
                    "filter_units": filter_units,
                }),
                EXCHANGE_TIMEOUT,
            )
            .await?;
        match parse_warning_render_reply(&reply, messages) {
            Ok(lines) => Ok(lines),
            Err(error) => {
                session.poisoned = true;
                session.verified_fingerprint = None;
                let _ = self.reap_poisoned(&mut session).await;
                Err(error)
            }
        }
    }

    async fn ensure_preflight(&self, session: &mut Session) -> WorkerResult<String> {
        if let Some(version) = session.verified_fingerprint.as_ref() {
            return Ok(version.clone());
        }
        if session.poisoned {
            self.reap_poisoned(session).await?;
        }

        let reply = self
            .exchange_locked(session, "ping", json!({}), PREFLIGHT_TIMEOUT)
            .await?;
        match parse_ping_reply(&reply).and_then(require_expected_fingerprint) {
            Ok(fingerprint) => {
                session.verified_fingerprint = Some(fingerprint.clone());
                Ok(fingerprint)
            }
            Err(error) => {
                session.poisoned = true;
                session.verified_fingerprint = None;
                let _ = self.reap_poisoned(session).await;
                Err(error)
            }
        }
    }

    async fn ensure_process(&self, session: &mut Session) -> WorkerResult<()> {
        if session.poisoned {
            self.reap_poisoned(session).await?;
        }
        if session.process.is_some() {
            return Ok(());
        }
        if session.bundle.is_none() {
            session.bundle = Some(PrivateBundle::materialize()?);
        }
        let bundle = session.bundle.as_ref().ok_or_else(|| {
            startup_error("private Bun policy semantics worker bundle was not materialized")
        })?;
        let mut command = Command::new(&self.executable);
        command
            .arg("--no-warnings")
            .arg(bundle.entry())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .current_dir(bundle.root());
        command.env_clear();
        command.envs(worker_environment(bundle, |name| std::env::var_os(name)));
        let mut child = command.spawn().map_err(|error| {
            startup_error(format!(
                "could not start configured Bun executable: {error}"
            ))
        })?;
        let Some(stdin) = child.stdin.take() else {
            terminate_child_sync(&mut child);
            return Err(startup_error(
                "Bun policy semantics worker did not expose stdin",
            ));
        };
        let Some(stdout) = child.stdout.take() else {
            terminate_child_sync(&mut child);
            return Err(startup_error(
                "Bun policy semantics worker did not expose stdout",
            ));
        };
        session.process = Some(Process {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
        });
        Ok(())
    }

    async fn exchange_locked(
        &self,
        session: &mut Session,
        action: &str,
        payload: Value,
        deadline: Duration,
    ) -> WorkerResult<Reply> {
        self.ensure_process(session).await?;
        let id = session.next_request_id.max(1);
        session.next_request_id = id.wrapping_add(1).max(1);
        let mut request = match payload {
            Value::Object(fields) => fields,
            _ => return Err(protocol_error("request payload must be an object")),
        };
        request.insert("id".to_owned(), json!(id));
        request.insert("action".to_owned(), json!(action));
        let mut line = serde_json::to_vec(&Value::Object(request))
            .map_err(|error| protocol_error(format!("request encoding failed: {error}")))?;
        line.push(b'\n');

        // If this future is canceled, or a write/read/protocol step fails, the
        // next caller must kill/reap this process before reusing the stream.
        let mut guard = ExchangePoisonGuard::new(session);
        let result = timeout(deadline, exchange_process(guard.session, &line, id)).await;
        let result = match result {
            Ok(result) => result,
            Err(_) => Err(NativePolicyRegexWorkerError::TimedOut),
        };
        if result.is_ok() {
            guard.complete();
        }
        result
    }

    async fn reap_poisoned(&self, session: &mut Session) -> WorkerResult<()> {
        if !session.poisoned {
            return Ok(());
        }
        if let Some(process) = session.process.as_mut() {
            let _ = process.child.start_kill();
            match timeout(REAP_TIMEOUT, process.child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(_)) => {
                    return Err(NativePolicyRegexWorkerError::Exited { status: None });
                }
                Err(_) => return Err(NativePolicyRegexWorkerError::TimedOut),
            }
        }
        session.process.take();
        session.bundle.take();
        session.poisoned = false;
        session.verified_fingerprint = None;
        Ok(())
    }
}

impl Drop for BunPolicyJsSemanticsWorker {
    fn drop(&mut self) {
        let session = self.session.get_mut();
        let stopped = if let Some(process) = session.process.as_mut() {
            terminate_child_sync(&mut process.child)
        } else {
            true
        };
        session.process.take();
        if stopped {
            session.bundle.take();
        } else if let Some(bundle) = session.bundle.take() {
            // Do not remove a bundle that a failed-to-stop child might still
            // be reading. The process is also configured with kill_on_drop.
            std::mem::forget(bundle);
        }
    }
}

async fn exchange_process(session: &mut Session, line: &[u8], id: u64) -> WorkerResult<Reply> {
    let process = session
        .process
        .as_mut()
        .ok_or_else(|| protocol_error("Bun policy semantics worker process is absent"))?;
    process
        .stdin
        .write_all(line)
        .await
        .map_err(|_| NativePolicyRegexWorkerError::Exited {
            status: process
                .child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code()),
        })?;
    process
        .stdin
        .flush()
        .await
        .map_err(|_| NativePolicyRegexWorkerError::Exited {
            status: process
                .child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code()),
        })?;
    let line = process
        .stdout
        .next_line()
        .await
        .map_err(|error| protocol_error(format!("response read failed: {error}")))?
        .ok_or_else(|| NativePolicyRegexWorkerError::Exited {
            status: process
                .child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code()),
        })?;
    let reply: Value = serde_json::from_str(&line)
        .map_err(|error| protocol_error(format!("worker returned invalid JSON: {error}")))?;
    validate_response_envelope(&reply, id)?;
    Ok(reply)
}

fn validate_response_envelope(reply: &Reply, expected_id: u64) -> WorkerResult<()> {
    let response_id = reply.get("id").and_then(Value::as_u64);
    if response_id != Some(expected_id) {
        return Err(protocol_error("response id did not match request"));
    }
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        let message = reply
            .get("error_message")
            .and_then(Value::as_str)
            .unwrap_or("worker rejected a well-formed request");
        return Err(protocol_error(message.to_owned()));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BunFingerprint {
    version: String,
    revision: String,
}

impl BunFingerprint {
    fn label(&self) -> String {
        format!("{}@{}", self.version, self.revision)
    }
}

fn parse_ping_reply(reply: &Reply) -> WorkerResult<BunFingerprint> {
    if reply.get("runtime").and_then(Value::as_str) != Some("bun")
        || reply.get("worker_id").and_then(Value::as_str) != Some(WORKER_ID)
        || reply.get("protocol_version").and_then(Value::as_u64) != Some(PROTOCOL_VERSION)
    {
        return Err(protocol_error("worker preflight fingerprint is malformed"));
    }
    let version = reply
        .get("bun_version")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| protocol_error("worker did not report Bun.version"))?;
    let revision = reply
        .get("bun_revision")
        .and_then(Value::as_str)
        .filter(|revision| !revision.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| protocol_error("worker did not report Bun.revision"))?;
    Ok(BunFingerprint { version, revision })
}

fn require_expected_fingerprint(fingerprint: BunFingerprint) -> WorkerResult<String> {
    let actual = fingerprint.label();
    if actual == EXPECTED_BUN_FINGERPRINT
        && fingerprint.version == EXPECTED_BUN_VERSION
        && fingerprint.revision == EXPECTED_BUN_REVISION
    {
        Ok(actual)
    } else {
        Err(NativePolicyRegexWorkerError::VersionMismatch {
            expected: EXPECTED_BUN_FINGERPRINT.to_owned(),
            actual,
        })
    }
}

fn unique_patterns(patterns: &[Vec<u16>]) -> Vec<Vec<u16>> {
    let mut seen = HashSet::with_capacity(patterns.len());
    let mut unique = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        if seen.insert(pattern.clone()) {
            unique.push(pattern.clone());
        }
    }
    unique
}

fn parse_validation_reply(
    reply: &Reply,
    expected_patterns: &[Vec<u16>],
) -> WorkerResult<Vec<NativePolicyRegexValidationRow>> {
    let rows = reply
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("worker omitted validation rows"))?;
    if rows.len() != expected_patterns.len() {
        return Err(protocol_error("worker returned the wrong number of rows"));
    }
    let mut parsed = Vec::with_capacity(rows.len());
    for (index, (row, expected_pattern)) in rows.iter().zip(expected_patterns).enumerate() {
        let pattern_units = read_units(row.get("pattern_units"))?;
        if pattern_units.as_slice() != expected_pattern.as_slice() {
            return Err(protocol_error(format!(
                "worker row {index} returned a different UTF-16 pattern key"
            )));
        }
        let outcome = row
            .get("outcome")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol_error(format!("worker row {index} omitted its outcome")))?;
        let parsed_outcome = match outcome.get("kind").and_then(Value::as_str) {
            Some("valid") => {
                let source_units = read_units(outcome.get("source_units"))?;
                let capture_count = outcome
                    .get("capture_count")
                    .and_then(Value::as_u64)
                    .and_then(|count| usize::try_from(count).ok())
                    .ok_or_else(|| {
                        protocol_error(format!("worker row {index} has no capture count"))
                    })?;
                NativePolicyRegexValidationOutcome::Valid {
                    source_units,
                    capture_count,
                }
            }
            Some("invalid") => NativePolicyRegexValidationOutcome::Invalid {
                error_message_units: read_units(outcome.get("error_message_units"))?,
            },
            _ => {
                return Err(protocol_error(format!(
                    "worker row {index} has an unknown outcome"
                )))
            }
        };
        parsed.push(NativePolicyRegexValidationRow {
            pattern_units,
            outcome: parsed_outcome,
        });
    }
    Ok(parsed)
}

fn parse_warning_render_reply(
    reply: &Reply,
    expected_messages: &[Vec<u16>],
) -> WorkerResult<Vec<Option<Vec<u16>>>> {
    let rows = reply
        .get("rows")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("worker omitted warning-render rows"))?;
    if rows.len() != expected_messages.len() {
        return Err(protocol_error(
            "worker returned the wrong number of warning-render rows",
        ));
    }
    let mut parsed = Vec::with_capacity(rows.len());
    for (index, (row, expected_message)) in rows.iter().zip(expected_messages).enumerate() {
        let input_units = read_units(row.get("input_units"))?;
        if input_units.as_slice() != expected_message.as_slice() {
            return Err(protocol_error(format!(
                "worker warning row {index} returned a different UTF-16 input",
            )));
        }
        let line_units = match row.get("line_units") {
            Some(Value::Null) => None,
            Some(Value::Array(_)) => Some(read_units(row.get("line_units"))?),
            _ => {
                return Err(protocol_error(format!(
                    "worker warning row {index} omitted its rendered line",
                )))
            }
        };
        parsed.push(line_units);
    }
    Ok(parsed)
}

fn parse_semantics_reply(
    reply: &Reply,
    expected_urls: &[Vec<u16>],
    expected_displays: &[Vec<u16>],
) -> WorkerResult<NativePolicyJsSemanticsRegistry> {
    let url_rows = reply
        .get("url_rows")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("worker omitted URL semantic rows"))?;
    if url_rows.len() != expected_urls.len() {
        return Err(protocol_error(
            "worker returned the wrong number of URL rows",
        ));
    }
    let mut parsed_urls = Vec::with_capacity(url_rows.len());
    for (index, (row, expected_key)) in url_rows.iter().zip(expected_urls).enumerate() {
        let input_units = read_units(row.get("input_units"))?;
        if input_units.as_slice() != expected_key.as_slice() {
            return Err(protocol_error(format!(
                "worker URL row {index} returned a different UTF-16 key"
            )));
        }
        let raw_rdt_valid = read_bool(row.get("raw_rdt_valid"), "raw_rdt_valid")?;
        let raw_archive_refine_valid = read_bool(
            row.get("raw_archive_refine_valid"),
            "raw_archive_refine_valid",
        )?;
        let raw_archive_hostname_blocked = read_optional_bool(
            row.get("raw_archive_hostname_blocked"),
            "raw_archive_hostname_blocked",
        )?;
        let zod = row
            .get("zod_url")
            .and_then(Value::as_object)
            .ok_or_else(|| protocol_error(format!("worker URL row {index} omitted zod_url")))?;
        let zod_url = match zod.get("kind").and_then(Value::as_str) {
            Some("invalid") => NativePolicyZodUrlOutcome::Invalid,
            Some("valid") => NativePolicyZodUrlOutcome::Valid {
                zod_value_units: read_units(zod.get("zod_value_units"))?,
                href_units: read_units(zod.get("href_units"))?,
                protocol_units: read_units(zod.get("protocol_units"))?,
                username_units: read_units(zod.get("username_units"))?,
                password_units: read_units(zod.get("password_units"))?,
                host_units: read_units(zod.get("host_units"))?,
                hostname_units: read_units(zod.get("hostname_units"))?,
                port_units: read_units(zod.get("port_units"))?,
                pathname_units: read_units(zod.get("pathname_units"))?,
                search_units: read_units(zod.get("search_units"))?,
                hash_units: read_units(zod.get("hash_units"))?,
                origin_units: read_units(zod.get("origin_units"))?,
                rdt_zod_value_valid: read_bool(
                    zod.get("rdt_zod_value_valid"),
                    "rdt_zod_value_valid",
                )?,
                archive_zod_refine_valid: read_bool(
                    zod.get("archive_zod_refine_valid"),
                    "archive_zod_refine_valid",
                )?,
                archive_zod_hostname_blocked: read_optional_bool(
                    zod.get("archive_zod_hostname_blocked"),
                    "archive_zod_hostname_blocked",
                )?,
            },
            _ => {
                return Err(protocol_error(format!(
                    "worker URL row {index} has an unknown Zod URL outcome"
                )))
            }
        };
        parsed_urls.push(NativePolicyUrlSemanticRow {
            input_units,
            raw_rdt_valid,
            raw_archive_refine_valid,
            raw_archive_hostname_blocked,
            zod_url,
        });
    }

    let display_rows = reply
        .get("display_rows")
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("worker omitted display semantic rows"))?;
    if display_rows.len() != expected_displays.len() {
        return Err(protocol_error(
            "worker returned the wrong number of display rows",
        ));
    }
    let mut parsed_displays = Vec::with_capacity(display_rows.len());
    for (index, (row, expected_key)) in display_rows.iter().zip(expected_displays).enumerate() {
        let input_units = read_units(row.get("input_units"))?;
        if input_units.as_slice() != expected_key.as_slice() {
            return Err(protocol_error(format!(
                "worker display row {index} returned a different UTF-16 key"
            )));
        }
        parsed_displays.push(NativePolicyDisplayRow {
            input_units,
            output_units: read_units(row.get("output_units"))?,
        });
    }

    NativePolicyJsSemanticsRegistry::from_rows(parsed_urls, parsed_displays)
        .map_err(|_| protocol_error("worker returned duplicate semantic keys"))
}

fn read_bool(value: Option<&Value>, label: &str) -> WorkerResult<bool> {
    value
        .and_then(Value::as_bool)
        .ok_or_else(|| protocol_error(format!("worker returned a non-boolean {label}")))
}

fn read_optional_bool(value: Option<&Value>, label: &str) -> WorkerResult<Option<bool>> {
    match value {
        Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(protocol_error(format!(
            "worker returned a non-null/non-boolean {label}"
        ))),
    }
}

fn read_units(value: Option<&Value>) -> WorkerResult<Vec<u16>> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| protocol_error("worker UTF-16 field is not an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|unit| u16::try_from(unit).ok())
                .ok_or_else(|| protocol_error("worker returned an invalid UTF-16 code unit"))
        })
        .collect()
}

fn resolve_bun_executable() -> WorkerResult<PathBuf> {
    let configured = std::env::var_os(branding::MOD_BUN_EXECUTABLE_ENV).map(PathBuf::from);
    let path_entries = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    resolve_bun_executable_from(configured, &path_entries).ok_or_else(|| {
        NativePolicyRegexWorkerError::ExecutableUnavailable {
            message: format!(
                "set {} or provide Bun on PATH",
                branding::MOD_BUN_EXECUTABLE_ENV
            ),
        }
    })
}

fn resolve_bun_executable_from(
    configured: Option<PathBuf>,
    path_entries: &[PathBuf],
) -> Option<PathBuf> {
    let candidate = configured.unwrap_or_else(|| PathBuf::from("bun"));
    let components = candidate.components().collect::<Vec<_>>();
    let has_path_component = candidate.is_absolute()
        || components.len() > 1
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)));
    if has_path_component {
        executable_file(&candidate)
    } else {
        path_entries
            .iter()
            .map(|directory| directory.join(&candidate))
            .find_map(|path| executable_file(&path))
    }
}

fn executable_file(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    std::fs::canonicalize(path).ok()
}

struct PrivateBundle {
    root: PathBuf,
    entry: PathBuf,
    home: PathBuf,
    config: PathBuf,
    temp: PathBuf,
}

impl PrivateBundle {
    fn materialize() -> WorkerResult<Self> {
        let root = create_private_worker_directory()?;
        let result = (|| {
            let home = root.join("home");
            let config = root.join("bun-config");
            let temp = root.join("tmp");
            create_private_subdirectory(&home)?;
            create_private_subdirectory(&config)?;
            create_private_subdirectory(&temp)?;
            let entry = root.join("bun_policy_semantics_worker.mjs");
            write_private_asset(&entry, WORKER_ASSET)?;
            Ok(Self {
                root: root.clone(),
                entry,
                home,
                config,
                temp,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&root);
        }
        result
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn entry(&self) -> &Path {
        &self.entry
    }
}

impl Drop for PrivateBundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn create_private_worker_directory() -> WorkerResult<PathBuf> {
    for _ in 0..64 {
        let id = NEXT_BUNDLE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lingxi-policy-js-semantics-{}-{id}",
            std::process::id()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(startup_error(format!(
                    "create private worker directory: {error}"
                )))
            }
        }
    }
    Err(startup_error(
        "could not allocate a private worker directory",
    ))
}

fn create_private_subdirectory(path: &Path) -> WorkerResult<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|error| startup_error(format!("create private worker directory: {error}")))
}

fn write_private_asset(path: &Path, bytes: &[u8]) -> WorkerResult<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| startup_error(format!("create embedded worker asset: {error}")))?;
    file.write_all(bytes)
        .map_err(|error| startup_error(format!("write embedded worker asset: {error}")))?;
    file.flush()
        .map_err(|error| startup_error(format!("flush embedded worker asset: {error}")))
}

fn worker_environment(
    bundle: &PrivateBundle,
    host_value: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut values = vec![
        ("HOME".into(), bundle.home.as_os_str().to_owned()),
        (
            "BUN_CONFIG_DIR".into(),
            bundle.config.as_os_str().to_owned(),
        ),
        ("TMPDIR".into(), bundle.temp.as_os_str().to_owned()),
    ];
    #[cfg(windows)]
    values.extend([
        ("USERPROFILE".into(), bundle.home.as_os_str().to_owned()),
        ("TEMP".into(), bundle.temp.as_os_str().to_owned()),
        ("TMP".into(), bundle.temp.as_os_str().to_owned()),
    ]);
    for key in ["TZ", "LANG", "LC_ALL"] {
        if let Some(value) = host_value(key) {
            values.push((key.into(), value));
        }
    }
    values
}

fn terminate_child_sync(child: &mut Child) -> bool {
    let _ = child.start_kill();
    let deadline = std::time::Instant::now() + REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

fn startup_error(message: impl Into<String>) -> NativePolicyRegexWorkerError {
    NativePolicyRegexWorkerError::Startup {
        message: message.into(),
    }
}

fn protocol_error(message: impl Into<String>) -> NativePolicyRegexWorkerError {
    NativePolicyRegexWorkerError::Protocol {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_ping_reply, parse_semantics_reply, parse_validation_reply,
        require_expected_fingerprint, unique_patterns, validate_response_envelope, BunFingerprint,
        BunPolicyJsSemanticsWorker, EXPECTED_BUN_FINGERPRINT, EXPECTED_BUN_REVISION,
        EXPECTED_BUN_VERSION,
    };
    use lingxi_core::settings::native_policy_regex::{
        NativePolicyRegexValidationOutcome, NativePolicyRegexWorkerError,
    };
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn exact_version_and_revision_are_a_capability_fingerprint() {
        let wrong_version = BunFingerprint {
            version: "1.3.11".to_owned(),
            revision: "test-revision".to_owned(),
        };
        assert!(matches!(
            require_expected_fingerprint(wrong_version),
            Err(NativePolicyRegexWorkerError::VersionMismatch { expected, actual })
                if expected == EXPECTED_BUN_FINGERPRINT
                    && actual == "1.3.11@test-revision"
        ));
        let wrong_revision = BunFingerprint {
            version: EXPECTED_BUN_VERSION.to_owned(),
            revision: "different-revision".to_owned(),
        };
        assert!(matches!(
            require_expected_fingerprint(wrong_revision),
            Err(NativePolicyRegexWorkerError::VersionMismatch { expected, actual })
                if expected == EXPECTED_BUN_FINGERPRINT
                    && actual == "1.4.3@different-revision"
        ));
        let expected = BunFingerprint {
            version: EXPECTED_BUN_VERSION.to_owned(),
            revision: EXPECTED_BUN_REVISION.to_owned(),
        };
        assert_eq!(
            require_expected_fingerprint(expected).unwrap(),
            EXPECTED_BUN_FINGERPRINT
        );
        assert_eq!(
            parse_ping_reply(&json!({
                "runtime": "bun",
                "worker_id": "native-policy-js-semantics-2.1.295-v4",
                "protocol_version": 4,
                "bun_revision": EXPECTED_BUN_REVISION,
                "bun_version": EXPECTED_BUN_VERSION,
            }))
            .unwrap()
            .label(),
            EXPECTED_BUN_FINGERPRINT
        );
    }

    #[test]
    fn exact_utf16_patterns_deduplicate_first_seen_and_worker_rows_keep_keys() {
        let patterns = vec![vec![0xd800], vec![b'a' as u16], vec![0xd800]];
        let unique = unique_patterns(&patterns);
        assert_eq!(unique, vec![vec![0xd800], vec![b'a' as u16]]);
        let reply = json!({
            "rows": [
                { "pattern_units": [55296], "outcome": { "kind": "valid", "source_units": [92, 117, 100, 56, 48, 48], "capture_count": 0 } },
                { "pattern_units": [97], "outcome": { "kind": "invalid", "error_message_units": [98, 97, 100] } }
            ]
        });
        let rows = parse_validation_reply(&reply, &unique).expect("rows use exact keys");
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0].outcome,
            NativePolicyRegexValidationOutcome::Valid {
                capture_count: 0,
                ..
            }
        ));
        assert!(matches!(
            &rows[1].outcome,
            NativePolicyRegexValidationOutcome::Invalid { .. }
        ));
    }

    #[tokio::test]
    async fn native_policy_warning_rendering_redacts_and_preserves_utf16() {
        let worker = BunPolicyJsSemanticsWorker::new_from_host_environment()
            .expect("the configured fixed Bun executable must be available");
        let token = "policyHelper: token=sk-ant-abcdefghijklmnopqrstuvwxyz012345"
            .encode_utf16()
            .collect::<Vec<_>>();
        let mut multiline = "policyHelper: first\n".encode_utf16().collect::<Vec<_>>();
        multiline.push(0xd800);
        multiline.extend(" paired ".encode_utf16());
        multiline.extend([0xd83d, 0xde00]);
        let mut controls = "policyHelper: control".encode_utf16().collect::<Vec<_>>();
        controls.extend([0x0001, 0x007f, 0x2028]);
        controls.extend("x".encode_utf16());
        let filter = "policyhelper".encode_utf16().collect::<Vec<_>>();

        let lines = worker
            .render_policy_warnings(
                &[token.clone(), multiline.clone(), controls.clone()],
                Some(&filter),
            )
            .await
            .expect("the warning source slices must execute in the pinned Bun worker");
        assert_eq!(lines.len(), 3);
        let lines = lines
            .into_iter()
            .map(|line| {
                String::from_utf16(&line.expect("the Native filter allows policyHelper"))
                    .expect("formatter escapes lone units before the OS boundary")
            })
            .collect::<Vec<_>>();
        assert!(lines[0].contains("[WARN] policyHelper: token=[REDACTED]\n"));
        assert!(!lines[0].contains("sk-ant-"));
        assert!(lines[1].contains(r#"[WARN] "policyHelper: first\n\ud800 paired 😀""#));
        assert!(lines[2].contains(r#"control\u0001\u007f\u2028x"#));
        assert!(lines.iter().all(|line| line.ends_with('\n')));

        let excluded_filter = "mcp".encode_utf16().collect::<Vec<_>>();
        let rejected = worker
            .render_policy_warnings(&[token], Some(&excluded_filter))
            .await
            .expect("a filter rejection is not a worker failure");
        assert_eq!(rejected, vec![None]);
    }

    #[tokio::test]
    async fn external_pinned_worker_preflights_batches_and_returns_native_semantics() {
        let worker = BunPolicyJsSemanticsWorker::new_from_host_environment()
            .expect("external worker tests require the pinned Bun executable");
        assert_eq!(
            worker
                .preflight()
                .await
                .expect("pinned Bun fingerprint preflight"),
            EXPECTED_BUN_FINGERPRINT
        );
        let patterns = vec![
            vec![u16::from(b'('), u16::from(b'x'), u16::from(b')')],
            vec![u16::from(b'a')],
            vec![u16::from(b'[')],
            vec![0xd800],
            vec![u16::from(b'('), u16::from(b'x'), u16::from(b')')],
        ];
        let rows = worker
            .validate_patterns(&patterns)
            .await
            .expect("pinned Bun returns all pattern outcomes");
        assert_eq!(rows.len(), 4);
        assert!(matches!(
            &rows[0].outcome,
            NativePolicyRegexValidationOutcome::Valid {
                source_units,
                capture_count: 1,
            } if source_units == &vec![u16::from(b'('), u16::from(b'x'), u16::from(b')')]
        ));
        assert!(matches!(
            &rows[1].outcome,
            NativePolicyRegexValidationOutcome::Valid {
                capture_count: 0,
                ..
            }
        ));
        assert!(matches!(
            &rows[2].outcome,
            NativePolicyRegexValidationOutcome::Invalid { error_message_units }
                if !error_message_units.is_empty()
        ));
        assert_eq!(rows[3].pattern_units, vec![0xd800]);
        assert!(matches!(
            &rows[3].outcome,
            NativePolicyRegexValidationOutcome::Valid { .. }
        ));
    }

    #[tokio::test]
    async fn missing_executable_is_a_startup_failure_not_a_pattern_result() {
        let worker = BunPolicyJsSemanticsWorker::with_executable(PathBuf::from(
            "/path/that/does/not/exist/bun",
        ));
        assert!(matches!(
            worker.preflight().await,
            Err(NativePolicyRegexWorkerError::Startup { .. })
        ));
    }

    #[tokio::test]
    async fn external_worker_shutdown_removes_private_bundle() {
        let worker = BunPolicyJsSemanticsWorker::new_from_host_environment()
            .expect("external worker tests require the pinned Bun executable");
        worker
            .preflight()
            .await
            .expect("pinned Bun fingerprint preflight");
        let root = {
            let session = worker.session.lock().await;
            session
                .bundle
                .as_ref()
                .expect("started worker owns its private bundle")
                .root
                .clone()
        };
        assert!(root.exists());
        drop(worker);
        assert!(
            !root.exists(),
            "worker drop removes its private source bundle"
        );
    }

    #[test]
    fn malformed_or_mismatched_worker_fingerprint_is_protocol_failure() {
        let malformed = json!({ "runtime": "bun", "bun_version": "1.4.3" });
        assert!(matches!(
            parse_ping_reply(&malformed),
            Err(NativePolicyRegexWorkerError::Protocol { .. })
        ));
        let missing_revision = json!({
            "runtime": "bun",
            "worker_id": "native-policy-js-semantics-2.1.295-v4",
            "protocol_version": 4,
            "bun_version": "1.4.3",
        });
        assert!(matches!(
            parse_ping_reply(&missing_revision),
            Err(NativePolicyRegexWorkerError::Protocol { .. })
        ));
        let wrong_protocol = json!({
            "runtime": "bun",
            "worker_id": "native-policy-js-semantics-2.1.295-v4",
            "protocol_version": 2,
            "bun_revision": "test-revision",
            "bun_version": "1.4.3",
        });
        assert!(matches!(
            parse_ping_reply(&wrong_protocol),
            Err(NativePolicyRegexWorkerError::Protocol { .. })
        ));
    }

    #[test]
    fn reply_request_id_is_a_protocol_invariant() {
        assert!(validate_response_envelope(&json!({ "id": 9, "ok": true }), 9).is_ok());
        assert!(matches!(
            validate_response_envelope(&json!({ "id": 8, "ok": true }), 9),
            Err(NativePolicyRegexWorkerError::Protocol { .. })
        ));
    }
    #[test]
    fn url_invalid_is_a_typed_row_and_semantic_keys_are_lossless() {
        use lingxi_core::settings::native_policy_js_semantics::NativePolicyZodUrlOutcome;

        let url_keys = vec![vec![b'x' as u16, 0xd800], vec![b'n' as u16]];
        let display_keys = vec![vec![0xd83d, 0xdc69, 0x200d, 0xd83d, 0xdcbb]];
        let reply = json!({
            "url_rows": [
                {
                    "input_units": [120, 55296],
                    "raw_rdt_valid": false,
                    "raw_archive_refine_valid": false,
                    "raw_archive_hostname_blocked": null,
                    "zod_url": { "kind": "invalid" }
                },
                {
                    "input_units": [110],
                    "raw_rdt_valid": false,
                    "raw_archive_refine_valid": false,
                    "raw_archive_hostname_blocked": null,
                    "zod_url": {
                        "kind": "valid",
                        "zod_value_units": [110],
                        "href_units": [104, 116, 116, 112, 58, 47, 47, 110],
                        "protocol_units": [104, 116, 116, 112, 58],
                        "username_units": [],
                        "password_units": [],
                        "host_units": [110],
                        "hostname_units": [110],
                        "port_units": [],
                        "pathname_units": [47],
                        "search_units": [],
                        "hash_units": [],
                        "origin_units": [104, 116, 116, 112, 58, 47, 47, 110],
                        "rdt_zod_value_valid": false,
                        "archive_zod_refine_valid": false,
                        "archive_zod_hostname_blocked": null
                    }
                }
            ],
            "display_rows": [
                {
                    "input_units": [55357, 56425, 8205, 55357, 56507],
                    "output_units": [55357, 56425, 8205, 55357, 56507]
                }
            ]
        });
        let registry = parse_semantics_reply(&reply, &url_keys, &display_keys)
            .expect("well-shaped semantic rows parse");
        assert!(matches!(
            &registry.url(&url_keys[0]).unwrap().zod_url,
            NativePolicyZodUrlOutcome::Invalid
        ));
        assert_eq!(
            registry.url(&url_keys[1]).unwrap().input_units,
            vec![b'n' as u16]
        );
        assert_eq!(
            registry.display(&display_keys[0]).unwrap().output_units,
            display_keys[0]
        );
    }

    #[tokio::test]
    async fn external_worker_keeps_url_invalid_distinct_and_runs_display_semantics() {
        use lingxi_core::settings::native_policy_js_semantics::NativePolicyZodUrlOutcome;

        let worker = BunPolicyJsSemanticsWorker::new_from_host_environment()
            .expect("external worker tests require the pinned Bun executable");
        assert_eq!(worker.preflight().await.unwrap(), EXPECTED_BUN_FINGERPRINT);

        let public = "https://example.com/".encode_utf16().collect::<Vec<_>>();
        let loopback = "https://127.0.0.1/".encode_utf16().collect::<Vec<_>>();
        let invalid = "not a URL".encode_utf16().collect::<Vec<_>>();
        let emoji = "👩‍💻".encode_utf16().collect::<Vec<_>>();
        let registry = worker
            .observe_semantics(
                &[
                    public.clone(),
                    loopback.clone(),
                    invalid.clone(),
                    public.clone(),
                ],
                &[emoji.clone(), emoji.clone()],
            )
            .await
            .expect("semantic request succeeds independently of invalid URL rows");

        assert_eq!(registry.url_rows().len(), 3);
        assert_eq!(registry.display_rows().len(), 1);
        assert!(matches!(
            &registry.url(&invalid).unwrap().zod_url,
            NativePolicyZodUrlOutcome::Invalid
        ));
        let public_row = registry.url(&public).unwrap();
        assert!(matches!(
            &public_row.zod_url,
            NativePolicyZodUrlOutcome::Valid {
                archive_zod_refine_valid: true,
                archive_zod_hostname_blocked: Some(false),
                ..
            }
        ));
        let loopback_row = registry.url(&loopback).unwrap();
        assert!(matches!(
            &loopback_row.zod_url,
            NativePolicyZodUrlOutcome::Valid {
                archive_zod_refine_valid: false,
                archive_zod_hostname_blocked: Some(true),
                ..
            }
        ));
        assert_eq!(
            registry.display(&emoji).unwrap().output_units,
            emoji,
            "the fixed Segmenter path preserves a complete emoji ZWJ grapheme"
        );
    }
}
