//! Acceptance draft: persistent JSON-lines bridge to the source-bound Bun
//! Insights worker. This file is not yet wired into the Harness module tree.

use crate::insights::{
    array_source, error_source, object_source, source_field, source_is_string, source_string,
    InsightsFacetQueryPlan, InsightsRecommendationFacts, InsightsRenderedReport,
    InsightsSectionQuery, InsightsSourceExecutor, InsightsSummaryChunk,
};
use crate::insights_corpus::{
    InsightsCorpusSourceFunctions, InsightsFacetRow, InsightsSourceFuture, NativeSessionAnalysis,
    RawTranscriptSession,
};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::collections::HashMap;
use std::io::Write as _;
use std::ops::{Deref, DerefMut};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

#[path = "insights_embedded_assets.rs"]
mod embedded_assets;

type Source = Utf16JsonProjection;
const MIN_SUPPORTED_BUN: (u32, u32, u32) = (1, 3, 11);
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);
const REAP_TIMEOUT: Duration = Duration::from_secs(2);
static NEXT_WORKER_BUNDLE_ID: AtomicU64 = AtomicU64::new(1);

struct BunWorkerProcess {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
}

/// One serialized worker connection. The process remains alive for all
/// transcript, query-planning, aggregation, recommendation, and HTML actions
/// in a `/insights` invocation, so a large `eKt` read does not pay process
/// startup cost per session.
pub struct BunInsightsSourceExecutor {
    worker: Arc<BunInsightsWorker>,
}

/// The executor owns one persistent worker. The process-wide registry below
/// stores only Weak references, so the worker and its private bundle shut down
/// when the last service using this executable is dropped.
struct BunInsightsWorker {
    executable: PathBuf,
    session: Mutex<BunWorkerSession>,
    #[cfg(test)]
    test_entry_override: Option<PathBuf>,
}

/// All state which must move together with the one request/response stream.
/// In particular request IDs are allocated while `session` is locked and a
/// canceled or malformed exchange leaves `poisoned` set until the old child
/// has been killed and reaped.
struct BunWorkerSession {
    process: Option<BunWorkerProcess>,
    bundle: Option<PrivateWorkerBundle>,
    poisoned: bool,
    preflight_verified: bool,
    next_request_id: u64,
}

impl Default for BunWorkerSession {
    fn default() -> Self {
        Self {
            process: None,
            bundle: None,
            poisoned: false,
            preflight_verified: false,
            next_request_id: 1,
        }
    }
}

/// A failed or canceled wire exchange cannot leave the process available for
/// another ID. Drop runs synchronously when the request future is canceled.
struct ExchangePoisonGuard<'a> {
    session: &'a mut BunWorkerSession,
    complete: bool,
}

impl<'a> ExchangePoisonGuard<'a> {
    fn new(session: &'a mut BunWorkerSession) -> Self {
        Self {
            session,
            complete: false,
        }
    }

    fn complete(&mut self) {
        self.complete = true;
    }
}

impl Deref for ExchangePoisonGuard<'_> {
    type Target = BunWorkerSession;

    fn deref(&self) -> &Self::Target {
        self.session
    }
}

impl DerefMut for ExchangePoisonGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.session
    }
}

impl Drop for ExchangePoisonGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.session.poisoned = true;
            self.session.preflight_verified = false;
        }
    }
}

impl BunInsightsSourceExecutor {
    /// Reuse the configured ModHost Bun executable lookup: the explicit
    /// `LINGXI_MOD_BUN_EXECUTABLE` value wins, then the same `bun` PATH command.
    /// All worker source is embedded in this Runtime binary; no worktree, asar,
    /// or installed source-tree path is consulted.
    pub fn new_from_host_environment() -> Result<Self, Source> {
        let executable = resolve_bun_executable()?;
        static WORKERS: OnceLock<StdMutex<HashMap<PathBuf, Weak<BunInsightsWorker>>>> =
            OnceLock::new();
        let workers = WORKERS.get_or_init(|| StdMutex::new(HashMap::new()));
        let mut workers = workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        workers.retain(|_, worker| worker.strong_count() > 0);
        if let Some(worker) = workers.get(&executable).and_then(Weak::upgrade) {
            return Ok(Self { worker });
        }
        let worker = Arc::new(BunInsightsWorker::new(executable.clone()));
        workers.insert(executable, Arc::downgrade(&worker));
        Ok(Self { worker })
    }

    /// Validate the worker lazily. Desktop registers the provider only when a
    /// Bun executable is present; first invocation performs one ping/version
    /// check before any source action, without adding session-startup I/O.
    #[cfg(test)]
    pub async fn preflight(&self) -> Result<(), Source> {
        self.worker.preflight().await
    }

    async fn call(&self, action: &str, payload: Source) -> Result<Source, Source> {
        self.worker.call(action, payload).await
    }
}

impl BunInsightsWorker {
    fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            session: Mutex::new(BunWorkerSession::default()),
            #[cfg(test)]
            test_entry_override: None,
        }
    }

    #[cfg(test)]
    fn with_test_entry(executable: PathBuf, entry: PathBuf) -> Self {
        let mut session = BunWorkerSession::default();
        session.bundle = Some(
            PrivateWorkerBundle::test_only()
                .expect("test worker gets a private home/config/temp root"),
        );
        Self {
            executable,
            session: Mutex::new(session),
            test_entry_override: Some(entry),
        }
    }

    #[cfg(test)]
    async fn preflight(&self) -> Result<(), Source> {
        let mut session = self.session.lock().await;
        self.ensure_preflight(&mut session).await
    }

    async fn ensure_preflight(&self, session: &mut BunWorkerSession) -> Result<(), Source> {
        if session.poisoned {
            self.reap_poisoned(session).await?;
        }
        if session.preflight_verified {
            return Ok(());
        }
        let reply = timeout(
            PREFLIGHT_TIMEOUT,
            self.exchange_locked(session, "ping", object_source(Vec::new())?),
        )
        .await
        .map_err(|_| error_source("insights Bun worker preflight timed out"))??;
        if let Err(error) = validate_worker_reply(&reply) {
            session.poisoned = true;
            session.preflight_verified = false;
            return Err(error);
        }
        session.preflight_verified = true;
        Ok(())
    }

    async fn ensure_process(&self, session: &mut BunWorkerSession) -> Result<(), Source> {
        if session.poisoned {
            self.reap_poisoned(session).await?;
        }
        if session.process.is_none() {
            if session.bundle.is_none() {
                // Failed materialization is not cached; the next call may
                // retry after a transient filesystem or permission error.
                session.bundle = Some(PrivateWorkerBundle::materialize()?);
            }
            let bundle = session
                .bundle
                .as_ref()
                .ok_or_else(|| error_source("Insights worker private bundle is absent"))?;
            let entry = self.worker_entry(bundle);
            let mut command = Command::new(&self.executable);
            command
                .arg("--no-warnings")
                .arg(entry)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                // Protocol stdout is machine-readable; worker diagnostics do
                // not contaminate it and are retained by the host launcher.
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .current_dir(bundle.root());
            command.env_clear();
            command.envs(worker_environment(bundle, |key| std::env::var_os(key)));
            let mut child = command.spawn().map_err(|error| {
                error_source(format!("insights Bun worker failed to start: {error}"))
            })?;
            let Some(input) = child.stdin.take() else {
                terminate_child_sync(&mut child);
                return Err(error_source("insights Bun worker did not expose stdin"));
            };
            let Some(output) = child.stdout.take() else {
                terminate_child_sync(&mut child);
                return Err(error_source("insights Bun worker did not expose stdout"));
            };
            session.process = Some(BunWorkerProcess {
                child,
                input,
                output: BufReader::new(output).lines(),
            });
            session.preflight_verified = false;
        }
        Ok(())
    }

    #[cfg(test)]
    fn worker_entry<'a>(&'a self, bundle: &'a PrivateWorkerBundle) -> &'a Path {
        self.test_entry_override
            .as_deref()
            .unwrap_or_else(|| bundle.entry())
    }

    #[cfg(not(test))]
    fn worker_entry<'a>(&'a self, bundle: &'a PrivateWorkerBundle) -> &'a Path {
        bundle.entry()
    }

    async fn reap_poisoned(&self, session: &mut BunWorkerSession) -> Result<(), Source> {
        if !session.poisoned {
            return Ok(());
        }
        if let Some(process) = session.process.as_mut() {
            let _ = process.child.start_kill();
            match timeout(REAP_TIMEOUT, process.child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    return Err(error_source(format!(
                        "insights Bun worker could not be reaped: {error}"
                    )));
                }
                Err(_) => {
                    return Err(error_source("insights Bun worker did not exit after kill"));
                }
            }
        }
        session.process.take();
        session.poisoned = false;
        session.preflight_verified = false;
        Ok(())
    }

    async fn exchange_locked(
        &self,
        session: &mut BunWorkerSession,
        action: &str,
        payload: Source,
    ) -> Result<Source, Source> {
        self.ensure_process(session).await?;
        let id = session.next_request_id;
        session.next_request_id = session.next_request_id.wrapping_add(1).max(1);
        // Encoding happens under the same lock as process selection and ID
        // allocation, so wire order and request identity cannot diverge.
        let line = encode_request_line(id, action, payload)?;
        let mut guard = ExchangePoisonGuard::new(session);
        let result = self.exchange_process(&mut guard, &line, id).await;
        if result.is_ok() {
            guard.complete();
        }
        result
    }

    async fn exchange_process(
        &self,
        session: &mut BunWorkerSession,
        line: &[u8],
        id: u64,
    ) -> Result<Source, Source> {
        let process = session
            .process
            .as_mut()
            .ok_or_else(|| error_source("insights Bun worker process is absent"))?;
        process.input.write_all(line).await.map_err(|error| {
            error_source(format!("insights worker request write failed: {error}"))
        })?;
        process
            .input
            .flush()
            .await
            .map_err(|error| error_source(format!("insights worker flush failed: {error}")))?;
        let reply = process
            .output
            .next_line()
            .await
            .map_err(|error| {
                error_source(format!("insights worker response read failed: {error}"))
            })?
            .ok_or_else(|| {
                let status = process
                    .child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "worker closed stdout".to_owned());
                error_source(format!("insights worker stopped: {status}"))
            })?;
        let envelope = Source::parse(&reply).map_err(|error| {
            error_source(format!("insights worker returned invalid JSON: {error}"))
        })?;
        if envelope
            .at_pointer("/id")
            .and_then(JsonValueView::as_number)
            != Some(id as f64)
        {
            return Err(error_source(
                "insights worker response id did not match request",
            ));
        }
        if envelope.string_at("/kind").as_deref() == Some("worker_error") {
            return Err(source_field(&envelope, "/detail")
                .filter(source_is_string)
                .unwrap_or_else(|| error_source("insights worker action failed")));
        }
        source_field(&envelope, "/result")
            .ok_or_else(|| error_source("insights worker omitted action result"))
    }

    async fn call(&self, action: &str, payload: Source) -> Result<Source, Source> {
        let mut session = self.session.lock().await;
        if action != "ping" {
            self.ensure_preflight(&mut session).await?;
        }
        self.exchange_locked(&mut session, action, payload).await
    }
}

impl Drop for BunInsightsWorker {
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
        } else {
            tracing::warn!(
                "Insights Bun worker did not stop during executor drop; retaining its private bundle"
            );
            if let Some(bundle) = session.bundle.take() {
                std::mem::forget(bundle);
            }
        }
    }
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

fn validate_worker_reply(reply: &Source) -> Result<(), Source> {
    if reply.string_at("/runtime").as_deref() != Some("bun")
        || reply.string_at("/metadataModule").as_deref()
            != Some("metadata-source/insights-native-actions.mjs")
        || reply.string_at("/sourceModule").as_deref() != Some("insights_source_actions.mjs")
    {
        return Err(error_source("insights Bun worker source preflight failed"));
    }
    let version = reply
        .string_at("/bunVersion")
        .and_then(|value| parse_bun_version(&value))
        .ok_or_else(|| {
            error_source("insights Bun worker did not report a valid stable Bun version")
        })?;
    if version < MIN_SUPPORTED_BUN {
        return Err(error_source("insights requires Bun 1.3.11 or newer"));
    }
    let actions = reply
        .array_len("/actions")
        .ok_or_else(|| error_source("insights Bun worker omitted its action list"))?;
    for required in REQUIRED_WORKER_ACTIONS {
        let found = (0..actions).any(|index| {
            reply.string_at(&format!("/actions/{index}")).as_deref() == Some(*required)
        });
        if !found {
            return Err(error_source(format!(
                "insights Bun worker is missing source action {required}"
            )));
        }
    }
    Ok(())
}

const REQUIRED_WORKER_ACTIONS: &[&str] = &[
    "metadata_for_transcript",
    "format_transcript",
    "sort_sessions_by_start_time",
    "warmup_only",
    "aggregate",
    "plan_facet_query",
    "finish_facet_prompt",
    "parse_facet_result",
    "report_section_queries",
    "parse_report_section",
    "at_a_glance_query",
    "parse_at_a_glance",
    "recommendation",
    "render_report",
    "report_timestamp",
];

struct PrivateWorkerBundle {
    root: PathBuf,
    entry: PathBuf,
    home: PathBuf,
    config: PathBuf,
    temp: PathBuf,
}

impl PrivateWorkerBundle {
    fn materialize() -> Result<Self, Source> {
        use embedded_assets::INSIGHTS_WORKER_ASSETS;

        let root = create_private_worker_directory()?;
        let result = (|| {
            let (home, config, temp) = create_private_environment_directories(&root)?;
            for asset in INSIGHTS_WORKER_ASSETS {
                let relative = Path::new(asset.path);
                if relative.as_os_str().is_empty()
                    || relative
                        .components()
                        .any(|component| !matches!(component, Component::Normal(_)))
                {
                    return Err(error_source("Insights worker asset path is not relative"));
                }
                let path = root.join(relative);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        error_source(format!("create Insights worker asset directory: {error}"))
                    })?;
                }
                write_private_asset(&path, asset.bytes)?;
            }
            let entry = root.join("insights_worker.mjs");
            if !entry.is_file() {
                return Err(error_source(
                    "embedded Insights worker entrypoint is missing",
                ));
            }
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

    #[cfg(test)]
    fn test_only() -> Result<Self, Source> {
        let root = create_private_worker_directory()?;
        let result = (|| {
            let (home, config, temp) = create_private_environment_directories(&root)?;
            Ok(Self {
                entry: root.join("unused-test-entry.mjs"),
                root: root.clone(),
                home,
                config,
                temp,
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(root);
        }
        result
    }

    fn entry(&self) -> &Path {
        &self.entry
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for PrivateWorkerBundle {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.root.display(),
                    %error,
                    "failed to remove private Insights worker assets"
                );
            }
        }
    }
}

fn create_private_worker_directory() -> Result<PathBuf, Source> {
    for _ in 0..64 {
        let id = NEXT_WORKER_BUNDLE_ID.fetch_add(1, Ordering::Relaxed);
        let candidate =
            std::env::temp_dir().join(format!("lingxi-insights-{}-{id}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error_source(format!(
                    "create private Insights worker directory: {error}"
                )));
            }
        }
    }
    Err(error_source(
        "could not allocate a private Insights worker directory",
    ))
}

fn create_private_subdirectory(path: &Path) -> Result<(), Source> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|error| error_source(format!("create private Insights worker directory: {error}")))
}

fn create_private_environment_directories(
    root: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf), Source> {
    let home = root.join("home");
    let config = root.join("bun-config");
    let temp = root.join("tmp");
    create_private_subdirectory(&home)?;
    create_private_subdirectory(&config)?;
    create_private_subdirectory(&temp)?;
    Ok((home, config, temp))
}

fn worker_environment(
    bundle: &PrivateWorkerBundle,
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
    {
        values.extend([
            ("USERPROFILE".into(), bundle.home.as_os_str().to_owned()),
            ("TEMP".into(), bundle.temp.as_os_str().to_owned()),
            ("TMP".into(), bundle.temp.as_os_str().to_owned()),
        ]);
    }
    // These affect local time/locale formatting but are not authentication or
    // runtime-loader variables. PATH and all API/OAuth variables are omitted.
    for key in ["TZ", "LANG", "LC_ALL"] {
        if let Some(value) = host_value(key) {
            values.push((key.into(), value));
        }
    }
    values
}

fn write_private_asset(path: &Path, bytes: &[u8]) -> Result<(), Source> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| error_source(format!("create embedded Insights worker asset: {error}")))?;
    file.write_all(bytes)
        .map_err(|error| error_source(format!("write embedded Insights worker asset: {error}")))?;
    file.flush()
        .map_err(|error| error_source(format!("flush embedded Insights worker asset: {error}")))
}

fn parse_bun_version(value: &str) -> Option<(u32, u32, u32)> {
    let numeric = match value.split_once('+') {
        Some((numeric, build)) if valid_semver_identifiers(build) => numeric,
        Some(_) => return None,
        None => value,
    };
    if numeric.contains('-') {
        return None;
    }
    let mut components = numeric.split('.');
    let major = parse_semver_number(components.next()?)?;
    let minor = parse_semver_number(components.next()?)?;
    let patch = parse_semver_number(components.next()?)?;
    if components.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn parse_semver_number(value: &str) -> Option<u32> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    value.parse().ok()
}

fn resolve_bun_executable() -> Result<PathBuf, Source> {
    let configured = std::env::var_os(branding::MOD_BUN_EXECUTABLE_ENV).map(PathBuf::from);
    let path_entries = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    resolve_bun_executable_from(configured, &path_entries).ok_or_else(|| {
        error_source(format!(
            "Insights requires a Bun executable from {} or PATH",
            branding::MOD_BUN_EXECUTABLE_ENV
        ))
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

fn valid_semver_identifiers(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
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

fn encode_request_line(id: u64, action: &str, payload: Source) -> Result<Vec<u8>, Source> {
    // Utf16JsonProjection is an internal flat-arena carrier. Serializing it
    // with serde would put `{root,nodes}` on the wire instead of the represented
    // JavaScript value. Build the actual ordered request object and use the
    // projection's source-preserving JSON encoder so lone UTF-16 units remain
    // escaped JSON strings for Bun's JSON.parse.
    let request = object_source(vec![
        ("id".into(), number_source(id as f64)?),
        ("action".into(), string_source(action)),
        ("payload".into(), payload),
    ])?;
    let mut line = request
        .to_json_string()
        .map_err(|error| error_source(format!("insights worker request encode failed: {error}")))?;
    line.push('\n');
    Ok(line.into_bytes())
}

impl InsightsCorpusSourceFunctions for BunInsightsSourceExecutor {
    fn metadata_for_transcript<'a>(
        &'a self,
        transcript: RawTranscriptSession,
    ) -> InsightsSourceFuture<'a, Vec<NativeSessionAnalysis>> {
        Box::pin(async move {
            let payload = object_source(vec![
                ("transcript_path".into(), transcript.transcript_path),
                ("q4_open_results".into(), transcript.q4_open_results),
            ])?;
            let result = self.call("metadata_for_transcript", payload).await?;
            let count = result
                .array_len("")
                .ok_or_else(|| error_source("metadata worker result must be an array"))?;
            let mut rows = Vec::with_capacity(count);
            for index in 0..count {
                let row = result
                    .subprojection(&format!("/{index}"))
                    .map_err(|error| {
                        error_source(format!("metadata worker row invalid: {error}"))
                    })?;
                rows.push(NativeSessionAnalysis {
                    metadata: row.subprojection("/metadata").map_err(|error| {
                        error_source(format!("metadata worker metadata invalid: {error}"))
                    })?,
                    messages: row.subprojection("/messages").map_err(|error| {
                        error_source(format!("metadata worker messages invalid: {error}"))
                    })?,
                });
            }
            Ok(rows)
        })
    }

    fn sort_sessions_by_start_time<'a>(
        &'a self,
        sessions: &'a [Utf16JsonProjection],
    ) -> InsightsSourceFuture<'a, Vec<usize>> {
        Box::pin(async move {
            let payload = object_source(vec![("sessions".into(), array_source(sessions)?)])?;
            let result = self.call("sort_sessions_by_start_time", payload).await?;
            let count = result
                .array_len("")
                .ok_or_else(|| error_source("session sort worker result must be an array"))?;
            if count != sessions.len() {
                return Err(error_source(
                    "session sort worker returned an invalid index count",
                ));
            }
            let mut seen = vec![false; count];
            let mut order = Vec::with_capacity(count);
            for index in 0..count {
                let value = result
                    .at_pointer(&format!("/{index}"))
                    .and_then(JsonValueView::as_number)
                    .ok_or_else(|| error_source("session sort index must be numeric"))?;
                if !value.is_finite()
                    || value.fract() != 0.0
                    || value < 0.0
                    || value >= count as f64
                {
                    return Err(error_source("session sort index is out of range"));
                }
                let index = value as usize;
                if std::mem::replace(&mut seen[index], true) {
                    return Err(error_source("session sort index is duplicated"));
                }
                order.push(index);
            }
            Ok(order)
        })
    }

    fn format_transcript<'a>(
        &'a self,
        metadata: &'a Source,
        messages: &'a Source,
    ) -> InsightsSourceFuture<'a, Source> {
        Box::pin(async move {
            let payload = object_source(vec![
                ("metadata".into(), metadata.clone()),
                ("messages".into(), messages.clone()),
            ])?;
            let result = self.call("format_transcript", payload).await?;
            if !source_is_string(&result) {
                return Err(error_source("format worker result must be a string"));
            }
            Ok(result)
        })
    }

    fn warmup_only<'a>(
        &'a self,
        ordered_facets: &'a [InsightsFacetRow],
    ) -> InsightsSourceFuture<'a, Vec<bool>> {
        Box::pin(async move {
            let rows = ordered_facets
                .iter()
                .map(|row| {
                    object_source(vec![
                        ("session_id".into(), string_source(&row.session_id)),
                        ("value".into(), row.value.clone()),
                    ])
                })
                .collect::<Result<Vec<_>, _>>()?;
            let payload = object_source(vec![("facets".into(), array_source(&rows)?)])?;
            let result = self.call("warmup_only", payload).await?;
            let count = result
                .array_len("")
                .ok_or_else(|| error_source("warmup worker result must be an array"))?;
            let mut decisions = Vec::with_capacity(count);
            for index in 0..count {
                let item = result
                    .at_pointer(&format!("/{index}"))
                    .and_then(JsonValueView::as_bool)
                    .ok_or_else(|| error_source("warmup worker item must be boolean"))?;
                decisions.push(item);
            }
            Ok(decisions)
        })
    }

    fn aggregate<'a>(
        &'a self,
        sessions: &'a [Source],
        ordered_facets: &'a [InsightsFacetRow],
    ) -> InsightsSourceFuture<'a, Source> {
        Box::pin(async move {
            let rows = ordered_facets
                .iter()
                .map(|row| {
                    object_source(vec![
                        ("session_id".into(), string_source(&row.session_id)),
                        ("value".into(), row.value.clone()),
                    ])
                })
                .collect::<Result<Vec<_>, _>>()?;
            let payload = object_source(vec![
                ("sessions".into(), array_source(sessions)?),
                ("facets".into(), array_source(&rows)?),
            ])?;
            self.call("aggregate", payload).await
        })
    }
}

impl InsightsSourceExecutor for BunInsightsSourceExecutor {
    fn plan_facet_query<'a>(
        &'a self,
        session_id: &'a str,
        metadata: &'a Source,
        formatted_transcript: &'a Source,
    ) -> InsightsSourceFuture<'a, InsightsFacetQueryPlan> {
        Box::pin(async move {
            let payload = object_source(vec![
                ("session_id".into(), string_source(session_id)),
                ("metadata".into(), metadata.clone()),
                ("formatted_transcript".into(), formatted_transcript.clone()),
            ])?;
            let result = self.call("plan_facet_query", payload).await?;
            let direct_prompt = result
                .get_property("direct_prompt")
                .and_then(|_| result.subprojection("/direct_prompt").ok())
                .filter(source_is_string);
            let chunks_source = result
                .subprojection("/summary_chunks")
                .map_err(|error| error_source(format!("facet summary plan is invalid: {error}")))?;
            let chunk_count = chunks_source
                .array_len("")
                .ok_or_else(|| error_source("facet summary chunks must be an array"))?;
            let mut summary_chunks = Vec::with_capacity(chunk_count);
            for index in 0..chunk_count {
                let chunk = chunks_source
                    .subprojection(&format!("/{index}"))
                    .map_err(|error| {
                        error_source(format!("facet summary chunk invalid: {error}"))
                    })?;
                summary_chunks.push(InsightsSummaryChunk {
                    prompt: chunk.subprojection("/prompt").map_err(|error| {
                        error_source(format!("facet summary prompt invalid: {error}"))
                    })?,
                    fallback: chunk.subprojection("/fallback").map_err(|error| {
                        error_source(format!("facet summary fallback invalid: {error}"))
                    })?,
                });
            }
            Ok(InsightsFacetQueryPlan {
                direct_prompt,
                summary_chunks,
                opaque_plan: result,
            })
        })
    }

    fn finish_facet_prompt<'a>(
        &'a self,
        plan: &'a InsightsFacetQueryPlan,
        summaries: &'a [Source],
    ) -> InsightsSourceFuture<'a, Source> {
        Box::pin(async move {
            let payload = object_source(vec![
                ("plan".into(), plan.opaque_plan.clone()),
                ("summaries".into(), array_source(summaries)?),
            ])?;
            self.call("finish_facet_prompt", payload).await
        })
    }

    fn parse_facet_result<'a>(
        &'a self,
        session_id: &'a str,
        response: &'a Source,
    ) -> InsightsSourceFuture<'a, Option<Source>> {
        Box::pin(async move {
            let payload = object_source(vec![
                ("session_id".into(), string_source(session_id)),
                ("response".into(), response.clone()),
            ])?;
            nullable_result(self.call("parse_facet_result", payload).await?)
        })
    }

    fn report_section_queries<'a>(
        &'a self,
        aggregate: &'a Source,
        facets: &'a [InsightsFacetRow],
    ) -> InsightsSourceFuture<'a, Vec<InsightsSectionQuery>> {
        Box::pin(async move {
            let facets = facet_payload_rows(facets)?;
            let payload = object_source(vec![
                ("aggregate".into(), aggregate.clone()),
                ("facets".into(), array_source(&facets)?),
            ])?;
            let result = self.call("report_section_queries", payload).await?;
            let count = result
                .array_len("")
                .ok_or_else(|| error_source("report query plan must be an array"))?;
            let mut queries = Vec::with_capacity(count);
            for index in 0..count {
                let row = result
                    .subprojection(&format!("/{index}"))
                    .map_err(|error| {
                        error_source(format!("report query plan row invalid: {error}"))
                    })?;
                let name = row
                    .string_at("/name")
                    .ok_or_else(|| error_source("report query plan name must be a string"))?;
                let prompt = row.subprojection("/prompt").map_err(|error| {
                    error_source(format!("report query prompt invalid: {error}"))
                })?;
                let max_tokens = row
                    .at_pointer("/max_tokens")
                    .and_then(JsonValueView::as_number)
                    .and_then(|value| u32::try_from(value as u64).ok())
                    .ok_or_else(|| error_source("report query max_tokens must be a u32"))?;
                queries.push(InsightsSectionQuery {
                    name: name.to_owned(),
                    prompt,
                    max_tokens,
                });
            }
            Ok(queries)
        })
    }

    fn parse_report_section<'a>(
        &'a self,
        name: &'a str,
        response: &'a Source,
    ) -> InsightsSourceFuture<'a, Option<Source>> {
        Box::pin(async move {
            let payload = object_source(vec![
                ("name".into(), string_source(name)),
                ("response".into(), response.clone()),
            ])?;
            nullable_result(self.call("parse_report_section", payload).await?)
        })
    }

    fn at_a_glance_query<'a>(
        &'a self,
        aggregate: &'a Source,
        facets: &'a [InsightsFacetRow],
        sections: &'a [(String, Source)],
    ) -> InsightsSourceFuture<'a, Source> {
        Box::pin(async move {
            let facets = facet_payload_rows(facets)?;
            let sections = sections
                .iter()
                .map(|(name, value)| {
                    object_source(vec![
                        ("name".into(), string_source(name)),
                        ("value".into(), value.clone()),
                    ])
                })
                .collect::<Result<Vec<_>, _>>()?;
            let payload = object_source(vec![
                ("aggregate".into(), aggregate.clone()),
                ("facets".into(), array_source(&facets)?),
                ("sections".into(), array_source(&sections)?),
            ])?;
            self.call("at_a_glance_query", payload).await
        })
    }

    fn parse_at_a_glance<'a>(
        &'a self,
        response: &'a Source,
    ) -> InsightsSourceFuture<'a, Option<Source>> {
        Box::pin(async move {
            let payload = object_source(vec![("response".into(), response.clone())])?;
            nullable_result(self.call("parse_at_a_glance", payload).await?)
        })
    }

    fn recommendation<'a>(
        &'a self,
        aggregate: &'a Source,
        facts: &'a InsightsRecommendationFacts,
    ) -> InsightsSourceFuture<'a, Option<Source>> {
        Box::pin(async move {
            let facts = object_source(vec![
                (
                    "auto_mode_available".into(),
                    bool_source(facts.auto_mode_available),
                ),
                (
                    "current_session_is_auto".into(),
                    bool_source(facts.current_session_is_auto),
                ),
                (
                    "sandbox_auto_allow_active".into(),
                    bool_source(facts.sandbox_auto_allow_active),
                ),
                (
                    "current_session_sees_no_dialogs".into(),
                    bool_source(facts.current_session_sees_no_dialogs),
                ),
                (
                    "live_arbiter_is_auto_classifier".into(),
                    bool_source(facts.live_arbiter_is_auto_classifier),
                ),
                (
                    "setup_skill_available".into(),
                    bool_source(facts.setup_skill_available),
                ),
                (
                    "has_auto_mode_environment".into(),
                    bool_source(facts.has_auto_mode_environment),
                ),
            ])?;
            let payload = object_source(vec![
                ("aggregate".into(), aggregate.clone()),
                ("facts".into(), facts),
            ])?;
            nullable_result(self.call("recommendation", payload).await?)
        })
    }

    fn render_report<'a>(
        &'a self,
        aggregate: &'a Source,
        sections: &'a [(String, Source)],
        at_a_glance: Option<&'a Source>,
        recommendation: Option<&'a Source>,
    ) -> InsightsSourceFuture<'a, InsightsRenderedReport> {
        Box::pin(async move {
            let section_rows = sections
                .iter()
                .map(|(name, value)| {
                    object_source(vec![
                        ("name".into(), string_source(name)),
                        ("value".into(), value.clone()),
                    ])
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut fields = vec![
                ("aggregate".into(), aggregate.clone()),
                ("sections".into(), array_source(&section_rows)?),
                (
                    "at_a_glance".into(),
                    at_a_glance.cloned().unwrap_or_else(null_source),
                ),
            ];
            if let Some(recommendation) = recommendation {
                fields.push(("recommendation".into(), recommendation.clone()));
            } else {
                fields.push(("recommendation".into(), null_source()));
            }
            let result = self.call("render_report", object_source(fields)?).await?;
            Ok(InsightsRenderedReport {
                insights_json: result
                    .subprojection("/insights_json")
                    .map_err(|error| error_source(format!("report JSON invalid: {error}")))?,
                summary_header: result
                    .subprojection("/summary_header")
                    .map_err(|error| error_source(format!("report header invalid: {error}")))?,
                summary_text: result
                    .subprojection("/summary_text")
                    .map_err(|error| error_source(format!("report summary invalid: {error}")))?,
                html: result
                    .subprojection("/html")
                    .map_err(|error| error_source(format!("report HTML invalid: {error}")))?,
            })
        })
    }

    fn report_timestamp<'a>(&'a self) -> InsightsSourceFuture<'a, String> {
        Box::pin(async move {
            let result = self
                .call("report_timestamp", object_source(Vec::new())?)
                .await?;
            source_string(&result)
                .ok_or_else(|| error_source("report timestamp worker result must be a string"))
        })
    }
}

fn facet_payload_rows(facets: &[InsightsFacetRow]) -> Result<Vec<Source>, Source> {
    facets
        .iter()
        .map(|row| {
            object_source(vec![
                ("session_id".into(), string_source(&row.session_id)),
                ("value".into(), row.value.clone()),
            ])
        })
        .collect()
}

fn nullable_result(source: Source) -> Result<Option<Source>, Source> {
    if source.at_pointer("").is_some_and(JsonValueView::is_null) {
        Ok(None)
    } else {
        Ok(Some(source))
    }
}

fn string_source(value: &str) -> Source {
    let code_units = value.encode_utf16().collect::<Vec<_>>();
    Source::root_string(value.to_owned(), code_units).expect("Rust text is valid UTF-16")
}

fn number_source(value: f64) -> Result<Source, Source> {
    let mut builder = lingxi_core::types::utf16_json::Utf16JsonBuilder::new();
    let root = builder.push_number(value);
    builder
        .finish(root)
        .map_err(|error| error_source(format!("insights worker request number invalid: {error}")))
}

fn bool_source(value: bool) -> Source {
    Source::parse(if value { "true" } else { "false" }).expect("static bool JSON")
}

fn null_source() -> Source {
    Source::parse("null").expect("static null JSON")
}

#[cfg(test)]
mod rust_to_bun_wire_tests {
    use super::*;

    #[test]
    fn request_encoder_emits_the_ordered_source_json_value() {
        let payload = Source::parse(r#"{"second":1.25,"first":"\ud800"}"#)
            .expect("source projection with an isolated UTF-16 unit");
        let line = encode_request_line(7, "metadata_for_transcript", payload)
            .expect("actual worker request JSON");
        assert_eq!(
            String::from_utf8(line).expect("JSON wire is UTF-8"),
            "{\"id\":7,\"action\":\"metadata_for_transcript\",\"payload\":{\"second\":1.25,\"first\":\"\\ud800\"}}\n"
        );
    }

    #[tokio::test]
    async fn rust_source_projection_reaches_bun_as_json_and_keeps_lone_utf16() {
        let executor = BunInsightsSourceExecutor::new_from_host_environment()
            .expect("embedded Insights worker assets should materialize");
        executor
            .preflight()
            .await
            .expect("Bun worker ping and source action inventory should pass");

        let temp = create_private_worker_directory().expect("private transcript directory");
        let result = async {
            let transcript_path = temp.join("session.jsonl");
            write_private_asset(
                &transcript_path,
                include_bytes!("fixtures/insights-small-session.jsonl"),
            )?;
            let transcript = RawTranscriptSession {
                session_id: "ed32091e-d448-41c9-b7bd-e99bbd50a833".into(),
                transcript_path: string_source(&transcript_path.to_string_lossy()),
                transcript_mtime_ms: 0.0,
                file_size_bytes: include_bytes!("fixtures/insights-small-session.jsonl").len()
                    as u64,
                q4_open_results: Source::parse(
                    r#"{"fleetFork":false,"fleetBackground":false,"daemon":false,"skillDoctor":false,"logout":false}"#,
                )
                .map_err(|error| error_source(format!("q4 gate fixture projection: {error}")))?,
            };
            let rows = executor.metadata_for_transcript(transcript).await?;
            if rows.len() != 1
                || rows[0].metadata.string_at("/session_id").as_deref()
                    != Some("ed32091e-d448-41c9-b7bd-e99bbd50a833")
            {
                return Err(error_source(
                    "Rust-to-Bun metadata request did not return the source fixture row",
                ));
            }

            let sort_rows = vec![
                Source::parse(r#"{"start_time":"a"}"#)
                    .map_err(|error| error_source(format!("sort row projection: {error}")))?,
                Source::parse(r#"{"start_time":"Z"}"#)
                    .map_err(|error| error_source(format!("sort row projection: {error}")))?,
            ];
            if executor.sort_sessions_by_start_time(&sort_rows).await? != vec![1, 0] {
                return Err(error_source(
                    "Rust-to-Bun source sort did not preserve Native localeCompare order",
                ));
            }

            let messages = rows[0].messages.clone();
            let metadata = Source::parse(
                r#"{"session_id":"surrogate","start_time":"2026-10-11T00:00:00.000Z","project_path":"before\ud800after","duration_minutes":1}"#,
            )
            .map_err(|error| error_source(format!("source metadata projection: {error}")))?;
            let format_payload = object_source(vec![
                ("metadata".into(), metadata),
                ("messages".into(), messages),
            ])?;
            let formatted = executor.call("format_transcript", format_payload).await?;
            let units = formatted
                .string_units("")
                .ok_or_else(|| error_source("Bun formatter returned a non-string"))?;
            if !units
                .windows(3)
                .any(|window| window[0] == 0xd800 && window[1] == b'a' as u16 && window[2] == b'f' as u16)
            {
                return Err(error_source(
                    "Rust-to-Bun JSON transport did not preserve the isolated UTF-16 unit",
                ));
            }
            Ok::<(), Source>(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&temp);
        result.expect("Rust source projections should survive the actual Bun JSON-lines bridge");
    }
}

#[cfg(test)]
mod worker_lifecycle_tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::{OsStr, OsString};

    const FAKE_WORKER: &str = r#"
import { createInterface } from "node:readline";
import { appendFile, readFile } from "node:fs/promises";

const eventPath = __EVENT_PATH__;
const releasePath = __RELEASE_PATH__;
const mode = __MODE__;
const actions = __ACTIONS__;
let starts = 1;
try {
  const previous = await readFile(eventPath, "utf8");
  starts += previous.split(/\r?\n/).filter((line) => line.startsWith("start:")).length;
} catch {}
await appendFile(eventPath, `start:${starts}\n`);

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  const request = JSON.parse(line);
  await appendFile(eventPath, `wire:${request.id}:${request.action}\n`);
  if (request.action === "hold") {
    await appendFile(eventPath, `hold:${request.id}\n`);
    for (;;) {
      try {
        await readFile(releasePath);
        break;
      } catch {
        await new Promise((resolve) => setTimeout(resolve, 5));
      }
    }
  }
  if (request.action === "malformed") {
    process.stdout.write("{bad-json\n");
    continue;
  }
  if (request.action === "eof") process.exit(0);
  if (request.action === "echo" && request.payload?.tag === "slow") {
    await new Promise((resolve) => setTimeout(resolve, 40));
  }
  const responseId = request.id + (request.action === "wrong-id" ? 1 : 0);
  const bunVersion = mode === "preflight-retry" && starts === 1
    ? "1.3.11-rc.1"
    : Bun.version;
  const result = request.action === "ping"
    ? {
        runtime: "bun",
        bunVersion,
        metadataModule: "metadata-source/insights-native-actions.mjs",
        sourceModule: "insights_source_actions.mjs",
        actions,
      }
    : request.payload ?? null;
  process.stdout.write(`${JSON.stringify({ id: responseId, kind: "result", result })}\n`);
  await appendFile(eventPath, `sent:${request.id}\n`);
}
"#;

    fn test_bun() -> PathBuf {
        resolve_bun_executable()
            .expect("the existing Bun installation is required for worker tests")
    }

    fn test_root() -> PathBuf {
        create_private_worker_directory().expect("private worker test directory")
    }

    fn create_fake_worker(root: &Path, mode: &str) -> (PathBuf, PathBuf, PathBuf) {
        let event_path = root.join("events.log");
        let release_path = root.join("release-hold");
        let entry = root.join("fake-worker.mjs");
        let event_path = serde_json::to_string(&event_path.to_string_lossy().to_string()).unwrap();
        let release_path =
            serde_json::to_string(&release_path.to_string_lossy().to_string()).unwrap();
        let mode = serde_json::to_string(mode).unwrap();
        let actions = serde_json::to_string(REQUIRED_WORKER_ACTIONS).unwrap();
        let source = FAKE_WORKER
            .replace("__EVENT_PATH__", &event_path)
            .replace("__RELEASE_PATH__", &release_path)
            .replace("__MODE__", &mode)
            .replace("__ACTIONS__", &actions);
        std::fs::write(&entry, source).expect("fake Bun worker source");
        (entry, root.join("events.log"), root.join("release-hold"))
    }

    fn payload(tag: &str) -> Source {
        object_source(vec![("tag".into(), string_source(tag))]).expect("test payload")
    }

    fn events(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn wait_for_event(path: &Path, expected: &str) {
        timeout(Duration::from_secs(5), async {
            loop {
                if events(path).iter().any(|event| event.starts_with(expected)) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fake worker did not reach the expected protocol boundary");
    }

    fn startup_count(events: &[String]) -> usize {
        events
            .iter()
            .filter(|event| event.starts_with("start:"))
            .count()
    }

    fn wire_ids(events: &[String]) -> Vec<(u64, String)> {
        events
            .iter()
            .filter_map(|event| {
                let mut parts = event.splitn(3, ':');
                if parts.next()? != "wire" {
                    return None;
                }
                Some((parts.next()?.parse().ok()?, parts.next()?.to_owned()))
            })
            .collect()
    }

    #[tokio::test]
    async fn preflight_retries_and_uncertain_exchanges_restart_before_next_id() {
        let root = test_root();
        let (entry, event_path, release_path) = create_fake_worker(&root, "preflight-retry");
        let worker = Arc::new(BunInsightsWorker::with_test_entry(test_bun(), entry));

        assert!(
            worker.preflight().await.is_err(),
            "prerelease Bun is not stable support"
        );
        worker
            .preflight()
            .await
            .expect("a failed preflight must not be cached permanently");

        for action in ["malformed", "wrong-id", "eof"] {
            assert!(worker.call(action, payload(action)).await.is_err());
            let echoed = worker
                .call("echo", payload(action))
                .await
                .expect("a protocol error must restart before the next action");
            assert_eq!(echoed.string_at("/tag").as_deref(), Some(action));
        }

        let pending = {
            let worker = Arc::clone(&worker);
            tokio::spawn(async move { worker.call("hold", payload("cancelled")).await })
        };
        wait_for_event(&event_path, "hold:").await;
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        // Release the fake worker only after cancellation, then wait until its
        // response is queued. The next call must discard it with the old child.
        std::fs::write(&release_path, b"release").unwrap();
        wait_for_event(&event_path, "sent:12").await;
        let echoed = worker
            .call("echo", payload("after-cancel"))
            .await
            .expect("cancellation after write must poison and restart the worker");
        assert_eq!(echoed.string_at("/tag").as_deref(), Some("after-cancel"));

        let records = events(&event_path);
        assert_eq!(startup_count(&records), 6);
        let ids = wire_ids(&records);
        assert!(ids.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(ids.last().map(|(_, action)| action.as_str()), Some("echo"));
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn concurrent_requests_keep_ids_in_wire_order_and_reuse_one_child() {
        let root = test_root();
        let (entry, event_path, _) = create_fake_worker(&root, "normal");
        let worker = BunInsightsWorker::with_test_entry(test_bun(), entry);
        let (slow, fast) = tokio::join!(
            worker.call("echo", payload("slow")),
            worker.call("echo", payload("fast")),
        );
        assert_eq!(slow.unwrap().string_at("/tag").as_deref(), Some("slow"));
        assert_eq!(fast.unwrap().string_at("/tag").as_deref(), Some("fast"));

        let records = events(&event_path);
        assert_eq!(startup_count(&records), 1);
        let ids = wire_ids(&records);
        assert_eq!(
            ids.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            ids.iter()
                .map(|(_, action)| action.as_str())
                .collect::<Vec<_>>(),
            vec!["ping", "echo", "echo"]
        );

        let bundle_root = {
            let session = worker.session.try_lock().unwrap();
            session.bundle.as_ref().unwrap().root().to_owned()
        };
        assert!(bundle_root.exists());
        drop(worker);
        assert!(
            !bundle_root.exists(),
            "normal executor drop waits for the child before removing its private bundle"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_spawn_does_not_fill_the_process_slot_and_can_be_retried() {
        let root = test_root();
        let (entry, _, _) = create_fake_worker(&root, "normal");
        let mut worker = BunInsightsWorker::with_test_entry(root.join("missing-bun"), entry);
        assert!(worker.preflight().await.is_err());
        {
            let session = worker.session.lock().await;
            assert!(session.process.is_none());
            assert!(!session.preflight_verified);
            assert!(session.bundle.is_some());
        }

        worker.executable = test_bun();
        worker
            .preflight()
            .await
            .expect("spawn failure must not poison the lazy preflight cache");
        assert!(worker.session.lock().await.process.is_some());
        drop(worker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_environment_is_private_and_excludes_credentials_and_path() {
        let bundle = PrivateWorkerBundle::test_only().unwrap();
        let root = bundle.root().to_owned();
        let host = HashMap::from([
            ("TZ", OsString::from("UTC")),
            ("LANG", OsString::from("en_US.UTF-8")),
            ("LC_ALL", OsString::from("C")),
            ("PATH", OsString::from("/sensitive/path")),
            ("ANTHROPIC_API_KEY", OsString::from("secret")),
            ("CLAUDE_CODE_OAUTH_TOKEN", OsString::from("secret")),
            (
                branding::MOD_BUN_EXECUTABLE_ENV,
                OsString::from("/configured/bun"),
            ),
        ]);
        let environment = worker_environment(&bundle, |key| host.get(key).cloned());
        let names = environment
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<HashSet<_>>();
        let expected = HashSet::from([
            "HOME".to_owned(),
            "BUN_CONFIG_DIR".to_owned(),
            "TMPDIR".to_owned(),
            "TZ".to_owned(),
            "LANG".to_owned(),
            "LC_ALL".to_owned(),
        ]);
        #[cfg(windows)]
        let expected = {
            let mut expected = expected;
            expected.extend([
                "USERPROFILE".to_owned(),
                "TEMP".to_owned(),
                "TMP".to_owned(),
            ]);
            expected
        };
        assert_eq!(names, expected);
        assert!(!names.contains("PATH"));
        assert!(!names.contains("ANTHROPIC_API_KEY"));
        assert!(!names.contains("CLAUDE_CODE_OAUTH_TOKEN"));
        let values = environment
            .into_iter()
            .collect::<HashMap<OsString, OsString>>();
        assert_eq!(
            values.get(OsStr::new("HOME")).unwrap().as_os_str(),
            bundle.home.as_os_str()
        );
        assert_eq!(
            values
                .get(OsStr::new("BUN_CONFIG_DIR"))
                .unwrap()
                .as_os_str(),
            bundle.config.as_os_str()
        );
        assert_eq!(
            values.get(OsStr::new("TMPDIR")).unwrap().as_os_str(),
            bundle.temp.as_os_str()
        );
        #[cfg(unix)]
        for path in [&root, &bundle.home, &bundle.config, &bundle.temp] {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700,
                "worker root, home, config, and temp dirs are private"
            );
        }
        drop(bundle);
        assert!(!root.exists());
    }

    #[test]
    fn bun_version_requires_stable_semver_and_accepts_build_metadata() {
        assert_eq!(parse_bun_version("1.3.11"), Some((1, 3, 11)));
        assert_eq!(parse_bun_version("1.3.11+release.3"), Some((1, 3, 11)));
        assert_eq!(parse_bun_version("1.3.10"), Some((1, 3, 10)));
        assert_eq!(parse_bun_version("1.3.11-rc.1"), None);
        assert_eq!(parse_bun_version("1.4.0-canary.2+build"), None);
        assert_eq!(parse_bun_version("01.3.11"), None);
        assert_eq!(parse_bun_version("1.3.11.1"), None);
    }

    #[test]
    fn configured_bun_path_wins_over_path_and_invalid_override_does_not_fallback() {
        let root = test_root();
        let configured = root.join("selected-bun");
        let path_directory = root.join("path-bin");
        std::fs::create_dir(&path_directory).unwrap();
        let path_bun = path_directory.join("bun");
        write_test_executable(&configured);
        write_test_executable(&path_bun);
        let path_entries = [path_directory.clone()];

        assert_eq!(
            resolve_bun_executable_from(Some(configured.clone()), &path_entries),
            std::fs::canonicalize(&configured).ok()
        );
        assert_eq!(
            resolve_bun_executable_from(None, &path_entries),
            std::fs::canonicalize(&path_bun).ok()
        );
        assert_eq!(
            resolve_bun_executable_from(Some(root.join("missing-bun")), &path_entries),
            None,
            "an explicitly configured path is authoritative rather than silently falling back"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn write_test_executable(path: &Path) {
        std::fs::write(path, b"test executable path").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }
}
