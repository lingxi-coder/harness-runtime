//! Persistent Mod middleware worker for Claude Code compatible hooks modules.
//!
//! A separate process bounds a stuck plugin (including regexp backtracking),
//! while one worker per session keeps module variables alive across events.
//! The host mediates `next(e)` so a hook can wrap an asynchronous Rust action.

#[path = "mod_agent_api.rs"]
mod agent_api;
#[path = "mod_ui_client_source.rs"]
mod client_source;
#[path = "mod_ui_client_host.rs"]
mod client_ui_host;
#[path = "mods_scan.rs"]
mod scan;
#[path = "mod_ui.rs"]
mod ui;

pub use agent_api::{ModAgentSpawnContext, ModAgentSpawnInput, ModAgentSpawnSlotLease};

use client_source::{prepare_plugin_sources, ClientSurfaceRuntimeSources, PreparedClientModules};

use crate::mod_api_context::{
    native_tool_call_origin, ModApiCallerKey, ModApiOriginContexts, ResolvedModApiContext,
};
use base64::Engine as _;
use branding::{ENV_PREFIX, MOD_BUN_EXECUTABLE_ENV};
use fs2::FileExt as _;
use futures_util::future::{AbortHandle, Abortable};
use futures_util::stream::{BoxStream, FuturesUnordered, StreamExt};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonBuilder, Utf16JsonProjection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch, Mutex, RwLock};

fn static_projection(source: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(source).expect("static Mod JSON projection")
}

fn root_string_projection(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .expect("Rust string is valid UTF-16")
}

fn projection_from_string_units(units: &[u16]) -> Result<Utf16JsonProjection, ModError> {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
        .map_err(|error| ModError::Protocol(error.to_string()))
}

fn generated_projection(value: &Value) -> Result<Utf16JsonProjection, ModError> {
    Utf16JsonProjection::from_serde_value(value)
        .map_err(|error| ModError::Protocol(error.to_string()))
}

fn object_projection(
    fields: Vec<(String, Utf16JsonProjection)>,
) -> Result<Utf16JsonProjection, ModError> {
    Utf16JsonProjection::object(fields).map_err(|error| ModError::Protocol(error.to_string()))
}

fn set_projection_field(
    value: &mut Utf16JsonProjection,
    field: &str,
    child: Utf16JsonProjection,
) -> Result<(), ModError> {
    value
        .set_field(field, child)
        .map_err(|error| ModError::Protocol(error.to_string()))
}

fn projection_i64(value: &Utf16JsonProjection, pointer: &str) -> Option<i64> {
    let number = value.number_at(pointer)?;
    (number.is_finite()
        && number.fract() == 0.0
        && number >= i64::MIN as f64
        && number <= i64::MAX as f64)
        .then_some(number as i64)
}

const WORKER_SOURCE: &str = include_str!("mods_worker.mjs");
const IPC_CODEC_SOURCE: &str = include_str!("mods_ipc_codec.mjs");
const CLIENT_WORKER_SOURCE: &str = include_str!("mod_ui_client_worker.mjs");
const CLIENT_SURFACE_RUNTIME_SOURCE: &str = include_str!("mod_ui_client_helpers.mjs");
const HOOK_TYPES_SOURCE: &str = include_str!("mod_ui_hooks_types.mjs");
// A hook can spend 10 s of its own time and its catch handler gets another
// 1 s. Keep the process watchdog beyond both so the worker can fail through.
const WORKER_RESPONSE_BUDGET: Duration = Duration::from_secs(20);
const PROCESS_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const TOOL_CALL_ABORTED_MESSAGE: &str = "produced no result (aborted)";
const SEC_DEFAULT_PLUGIN: &str = "cc-plugin-sec-default";
const SEC_DEFAULT_STORAGE_ID: &str = "cc-plugin-sec-default@builtin";
const SEC_DEFAULT_HOOK_ID: u64 = 0;
const SEC_DEFAULT_CALLER_EVENT: &str = "internal.sec-default";

tokio::task_local! {
    static MOD_WORKER_RECOVERY_SCOPE: bool;
}

const WORKER_EPOCH_RUNNING: u8 = 0;
const WORKER_EPOCH_DEAD: u8 = 1;
const WORKER_EPOCH_RECOVERING: u8 = 2;
static STORE_TEMP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MOD_HOST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MOD_WORKER_BOOTSTRAP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MOD_TOOL_CALL_TRANSACTION_ID: AtomicU64 = AtomicU64::new(1);

struct ModWorkerBootstrap {
    directory: PathBuf,
    entry: PathBuf,
}

impl ModWorkerBootstrap {
    fn create() -> Result<Self, ModError> {
        let temp_root = std::env::temp_dir();
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                ModError::Unavailable(format!("system clock is before Unix epoch: {error}"))
            })?
            .as_nanos();
        let process_id = std::process::id();
        let mut directory = None;
        for _ in 0..64 {
            let id = NEXT_MOD_WORKER_BOOTSTRAP_ID.fetch_add(1, Ordering::Relaxed);
            let candidate =
                temp_root.join(format!("lingxi-mod-worker-{process_id}-{created_at}-{id}"));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            match builder.create(&candidate) {
                Ok(()) => {
                    directory = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(ModError::Unavailable(format!(
                        "create private Mod worker bootstrap directory: {error}"
                    )));
                }
            }
        }
        let directory = directory.ok_or_else(|| {
            ModError::Unavailable(
                "could not allocate a unique Mod worker bootstrap directory".into(),
            )
        })?;
        let entry = directory.join("mods_worker.mjs");
        let ipc_codec = directory.join("mods_ipc_codec.mjs");
        let client_worker = directory.join("mod_ui_client_worker.mjs");
        let write_result = (|| {
            Self::write_source(&entry, WORKER_SOURCE)?;
            Self::write_source(&ipc_codec, IPC_CODEC_SOURCE)?;
            Self::write_source(&client_worker, CLIENT_WORKER_SOURCE)
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }
        Ok(Self { directory, entry })
    }

    fn write_source(path: &Path, source: &str) -> Result<(), ModError> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| {
                ModError::Unavailable(format!(
                    "create Mod worker source file {}: {error}",
                    path.display()
                ))
            })?;
        file.write_all(source.as_bytes()).map_err(|error| {
            ModError::Unavailable(format!(
                "write Mod worker source file {}: {error}",
                path.display()
            ))
        })
    }
}

impl Drop for ModWorkerBootstrap {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.directory.display(), %error, "failed to remove owned Mod worker bootstrap directory");
            }
        }
    }
}

#[cfg(test)]
#[test]
fn bun_worker_bootstrap_materializes_and_cleans_only_its_source_directory() {
    let bootstrap = ModWorkerBootstrap::create().expect("Bun worker bootstrap");
    assert_eq!(
        std::fs::read_to_string(&bootstrap.entry).expect("worker entry source"),
        WORKER_SOURCE
    );
    assert_eq!(
        std::fs::read_to_string(bootstrap.directory.join("mods_ipc_codec.mjs"))
            .expect("worker IPC codec source"),
        IPC_CODEC_SOURCE
    );
    assert_eq!(
        std::fs::read_to_string(bootstrap.directory.join("mod_ui_client_worker.mjs"))
            .expect("Client UI module source"),
        CLIENT_WORKER_SOURCE
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&bootstrap.directory)
            .expect("private bootstrap directory metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
    let directory = bootstrap.directory.clone();
    drop(bootstrap);
    assert!(!directory.exists());
}

fn client_surface_runtime_sources() -> ClientSurfaceRuntimeSources {
    ClientSurfaceRuntimeSources {
        surface_runtime: CLIENT_SURFACE_RUNTIME_SOURCE.to_owned(),
        hooks_types: HOOK_TYPES_SOURCE.to_owned(),
    }
}

fn exact_string(input: &Utf16JsonProjection, pointer: &str) -> Option<String> {
    String::from_utf16(&input.string_units(pointer)?).ok()
}

fn lossy_string(input: &Utf16JsonProjection, pointer: &str) -> Option<String> {
    Some(String::from_utf16_lossy(&input.string_units(pointer)?))
}

fn js_trim_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028..=0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

fn js_trimmed_units(units: &[u16]) -> &[u16] {
    let start = units
        .iter()
        .position(|unit| !js_trim_whitespace(*unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|unit| !js_trim_whitespace(*unit))
        .map_or(start, |index| index + 1);
    &units[start..end]
}

fn env_name(event: &str, input: &Utf16JsonProjection) -> Result<String, ModError> {
    let name = exact_string(input, "/name")
        .filter(|name| !name.is_empty() && !name.bytes().any(|byte| byte == b'=' || byte == 0))
        .ok_or_else(|| ModError::Hook(format!("{event} takes a valid variable name")))?;
    Ok(name)
}

fn env_set_value(input: &Utf16JsonProjection) -> Result<Option<String>, ModError> {
    if input.at_pointer("/value").is_none() {
        return Ok(None);
    }
    let value = lossy_string(input, "/value")
        .filter(|value| !value.contains('\0'))
        .ok_or_else(|| ModError::Hook("env.set value must be a string without NUL".into()))?;
    Ok(Some(value))
}

struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            // The child starts its own process group. Descendants retaining an
            // output pipe must stop when a Mod call times out or is cancelled.
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

async fn read_process_output<R: AsyncRead + Unpin>(
    mut pipe: R,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut collected = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let count = pipe.read(&mut chunk).await?;
        if count == 0 {
            return Ok((collected, truncated));
        }
        let remaining = PROCESS_OUTPUT_LIMIT.saturating_sub(collected.len());
        collected.extend_from_slice(&chunk[..count.min(remaining)]);
        truncated |= count > remaining;
    }
}

fn decode_process_output(bytes: &[u8], truncated: bool) -> String {
    let mut end = bytes.len();
    if truncated && end > 0 {
        let mut lead = end;
        while lead > 0 && end - lead < 3 && bytes[lead - 1] & 0b1100_0000 == 0b1000_0000 {
            lead -= 1;
        }
        if lead > 0 {
            let first = bytes[lead - 1];
            let expected = match first {
                0b1100_0000..=0b1101_1111 => 2,
                0b1110_0000..=0b1110_1111 => 3,
                0b1111_0000..=0b1111_0111 => 4,
                _ => 1,
            };
            if expected > end - lead + 1 {
                end = lead - 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

async fn run_process_api(
    input: Utf16JsonProjection,
    session_cwd: &Path,
    plugin: &str,
    environment: &HashMap<String, Option<String>>,
) -> Result<Utf16JsonProjection, ModError> {
    let argv_count = input.array_len("/argv").ok_or_else(|| {
        ModError::Hook("takes argv, a non-empty list of strings naming the command first".into())
    })?;
    if argv_count == 0 {
        return Err(ModError::Hook(
            "takes argv, a non-empty list of strings naming the command first".into(),
        ));
    }
    let mut argv = Vec::with_capacity(argv_count);
    for index in 0..argv_count {
        let arg = lossy_string(&input, &format!("/argv/{index}")).ok_or_else(|| {
            ModError::Hook(
                "takes argv, a non-empty list of strings naming the command first".into(),
            )
        })?;
        argv.push(arg);
    }
    if argv[0].is_empty() {
        return Err(ModError::Hook(
            "takes argv, a non-empty list of strings naming the command first".into(),
        ));
    }
    let init = match input.at_pointer("/init") {
        None => None,
        Some(init) if init.object_entries().is_some() => Some(init),
        Some(_) => {
            return Err(ModError::Hook(
                "takes init, { cwd?, env?, stdin?, timeoutMs? }".into(),
            ));
        }
    };
    let cwd = match init.and_then(|init| init.get_property("cwd")) {
        None => session_cwd.to_path_buf(),
        Some(cwd) if cwd.code_units().is_some_and(|units| !units.is_empty()) => session_cwd.join(
            String::from_utf16_lossy(cwd.code_units().unwrap_or_default()),
        ),
        _ => return Err(ModError::Hook("init.cwd is a non-empty path".into())),
    };
    let env = match init.and_then(|init| init.get_property("env")) {
        None => None,
        Some(env) => {
            let entries = env
                .object_entries()
                .ok_or_else(|| ModError::Hook("init.env is an object of strings".into()))?;
            let mut values = Vec::with_capacity(entries.len());
            for entry in entries {
                let value = env
                    .get_property_units(entry.code_units)
                    .and_then(JsonValueView::code_units)
                    .ok_or_else(|| ModError::Hook("init.env is an object of strings".into()))?;
                values.push((
                    String::from_utf16_lossy(entry.code_units),
                    String::from_utf16_lossy(value),
                ));
            }
            Some(values)
        }
    };
    let stdin = match init.and_then(|init| init.get_property("stdin")) {
        None => None,
        Some(stdin) => Some(
            stdin
                .code_units()
                .map(String::from_utf16_lossy)
                .ok_or_else(|| ModError::Hook("init.stdin is a string".into()))?,
        ),
    };
    let timeout_ms = match init.and_then(|init| init.get_property("timeoutMs")) {
        None => 30_000,
        Some(value) => value
            .as_number()
            .filter(|number| number.fract() == 0.0 && *number > 0.0 && *number <= 600_000.0)
            .map(|number| number as u64)
            .ok_or_else(|| {
                ModError::Hook("init.timeoutMs is a whole number of ms, 1 to 600000".into())
            })?,
    };
    let exe = &argv[0];
    let basename = Path::new(exe)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_git = matches!(
        basename.as_str(),
        "git" | "git.exe" | "git.cmd" | "git.bat" | "git.com"
    );
    let mut command = Command::new(exe);
    if is_git {
        let null_device = if cfg!(windows) {
            r"\\.\NUL"
        } else {
            "/dev/null"
        };
        for (key, value) in [
            ("core.fsmonitor", ""),
            ("core.hooksPath", null_device),
            ("core.askPass", ""),
            ("protocol.ext.allow", "never"),
            ("submodule.recurse", "false"),
            ("log.showSignature", "false"),
            ("format.pretty", "medium"),
            ("gc.auto", "0"),
            ("maintenance.auto", "false"),
        ] {
            command.arg("-c").arg(format!("{key}={value}"));
        }
        let mut args = argv[1..].to_vec();
        for index in 0..args.len().saturating_sub(1) {
            if args[index] == "-c"
                && args[index + 1]
                    .to_ascii_lowercase()
                    .starts_with("core.hookspath=")
            {
                args[index + 1] = format!("core.hooksPath={null_device}");
            }
        }
        command.args(&args);
        command.env("GIT_NO_LAZY_FETCH", "1");
        command.env("GIT_NO_REPLACE_OBJECTS", "1");
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("GIT_PROXY_COMMAND", "");
    } else {
        command.args(&argv[1..]);
    }
    for (key, value) in environment {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }
    if let Some(env) = env {
        for (key, value) in env {
            command.env(key, value);
        }
    }
    command
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        ModError::Hook(format!(
            "{plugin}: $.process.run({exe}) failed to start: {error}"
        ))
    })?;
    let mut process_group = ProcessGroupGuard { pid: child.id() };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ModError::Protocol("process stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ModError::Protocol("process stderr unavailable".into()))?;
    let mut input_pipe = child.stdin.take();
    let write_stdin = async move {
        if let Some(mut pipe) = input_pipe.take() {
            if let Some(stdin) = stdin {
                // Claude's Node host ignores stdin EPIPE when a child exits
                // without consuming input; its exit status and output still win.
                let _ = pipe.write_all(stdin.as_bytes()).await;
            }
            let _ = pipe.shutdown().await;
        }
        Ok::<(), std::io::Error>(())
    };
    let work = async {
        let (_, stdout, stderr, status) = tokio::try_join!(
            write_stdin,
            read_process_output(stdout),
            read_process_output(stderr),
            child.wait(),
        )?;
        Ok::<_, std::io::Error>((stdout, stderr, status))
    };
    let (stdout, stderr, status) =
        match tokio::time::timeout(Duration::from_millis(timeout_ms), work).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(ModError::Io(error)),
            Err(_) => {
                return Err(ModError::Hook(format!(
                    "{plugin}: $.process.run({exe}) aborted: still running after {timeout_ms}ms"
                )));
            }
        };
    process_group.pid = None;
    generated_projection(&json!({
        "exitCode":status.code().unwrap_or(1),
        "stdout":decode_process_output(&stdout.0, stdout.1),
        "stderr":decode_process_output(&stderr.0, stderr.1),
        "isStdoutTruncated":stdout.1,
        "isStderrTruncated":stderr.1,
    }))
}

#[derive(Debug, Error)]
pub enum ModError {
    #[error("mod worker unavailable: {0}")]
    Unavailable(String),
    #[error("mod worker I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("mod worker protocol: {0}")]
    Protocol(String),
    #[error("mod hook failed: {0}")]
    Hook(String),
    #[error("mod hook failed with source text")]
    HookSource(Utf16JsonProjection),
    #[error("{0}")]
    Native(String),
    #[error("mod hook exceeded its time budget")]
    Timeout,
}

impl ModError {
    /// Exact source text for failures originating in a Mod API call.
    #[must_use]
    pub fn source_projection(&self) -> Option<&Utf16JsonProjection> {
        match self {
            Self::HookSource(source) => Some(source),
            _ => None,
        }
    }
}

/// Strip unknown Native UI envelope fields and validate one canonical control
/// request. Hosts use this even when no Mod worker is attached so fallback
/// responses follow the same request schema.
pub fn normalize_mod_ui_control_request(
    input: lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, ModError> {
    client_ui_host::normalize_control_request(&input)
}

/// A Mod chain's result and the plugin names whose matching `tool.check` or
/// `command.run` handlers were eligible to decide it.
pub struct ModDispatchOutcome {
    pub result_projection: Utf16JsonProjection,
    pub hooked: Vec<String>,
    pub all_hooked_builtin: bool,
    /// Source-matched `ui.render` participants, carried only as internal worker
    /// dispatch metadata for the Client failure state token.
    pub ui_render_plugins: Vec<String>,
    /// Worker-measured elapsed milliseconds for the `ui.render` evaluator.
    /// Native's slow-plugin marker is based on evaluation time, so this must
    /// not include the Rust-side worker IPC round trip.
    pub ui_render_duration_ms: Option<u64>,
}

/// Host-selected plugin and UI routing metadata for one typed Mod dispatch.
#[derive(Debug, Clone, Default)]
pub struct ModDispatchScope {
    pub plugin_scope: Option<String>,
    pub press_token: Option<Utf16JsonProjection>,
    pub ui_render_host_revision: Option<u64>,
}

enum HostApiValue {
    JsonProjection(Utf16JsonProjection),
    Undefined,
}

impl HostApiValue {
    fn reply(self, request_id: u64, call_id: u64) -> Result<Utf16JsonProjection, ModError> {
        let mut fields = vec![
            ("id".to_owned(), projection_u64_value(request_id)?),
            ("kind".to_owned(), projection_string_value("api.result")?),
            ("callId".to_owned(), projection_u64_value(call_id)?),
        ];
        if let Self::JsonProjection(result) = self {
            fields.push(("result".to_owned(), result));
        }
        projection_object(fields)
    }
}

/// Host-minted identity for one `$.tool.call` API transaction. Every core run
/// reached through that API call's middleware `next` chain shares this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModToolCallTransactionId(u64);

impl ModToolCallTransactionId {
    /// Numeric key for host-owned transaction stores. Worker input cannot set
    /// or replace this identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Known source discriminator for the Projects-session feature result.
/// `Unknown` is reserved for an explicitly present host observation whose
/// source could not be classified; an unavailable resolver is `None` in
/// [`ProjectsConsentFacts::feature_result`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProjectsFeatureSource {
    Override,
    Disabled,
    Payload,
    Fallback,
    Disk,
    #[default]
    Unknown,
}

/// Value and source returned by the host's GrowthBook feature resolver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectsFeatureResult {
    pub value: bool,
    pub source: ProjectsFeatureSource,
}

/// Trusted, source-separated inputs used by the Native Projects-session
/// predicate. `None` means the host has not acquired an authoritative value;
/// it is not interchangeable with `Some(false)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectsConsentFacts {
    /// `Is(el).value`: the default-host sticky latch, not a session-kind flag.
    pub default_host_sticky_latch: Option<bool>,
    /// Parsed `CLAUDE_CODE_PROJECTS_SESSION` value.
    pub projects_env: Option<bool>,
    /// `Sbt(SS())` observation from the live session MCP state.
    pub session_mcp_signal: Option<bool>,
    /// Authoritative feature value/source pair. Native always returns a pair;
    /// `None` records that this host could not acquire an equivalent result.
    pub feature_result: Option<ProjectsFeatureResult>,
    /// Whether GrowthBook has used a non-default API host.
    pub growthbook_used_non_default_host: Option<bool>,
}

/// Host-only result of Native-ordered `$.tool.call` preparation. The resolved
/// tool is retained as an opaque, reusable handle so each middleware `next`
/// run dispatches the same catalog entry without resolving its name again.
#[derive(Clone)]
pub struct PreparedModToolCall {
    canonical_tool_name: String,
    consent: Option<String>,
    projects_consent: ProjectsConsentFacts,
    resolved_tool: Arc<dyn Any + Send + Sync>,
}

impl PreparedModToolCall {
    /// Create a prepared call from a host-owned resolver snapshot. The opaque
    /// value is never serialized to or decoded from worker JSON.
    #[must_use]
    pub fn new<T>(
        canonical_tool_name: String,
        consent: Option<String>,
        projects_consent: ProjectsConsentFacts,
        resolved_tool: T,
    ) -> Self
    where
        T: Any + Send + Sync + 'static,
    {
        Self {
            canonical_tool_name,
            consent,
            projects_consent,
            resolved_tool: Arc::new(resolved_tool),
        }
    }

    #[must_use]
    pub fn canonical_tool_name(&self) -> &str {
        &self.canonical_tool_name
    }

    #[must_use]
    pub fn consent(&self) -> Option<&str> {
        self.consent.as_deref()
    }

    #[must_use]
    pub fn projects_consent(&self) -> &ProjectsConsentFacts {
        &self.projects_consent
    }

    #[must_use]
    pub fn resolved_tool<T>(&self) -> Option<&T>
    where
        T: Any + Send + Sync + 'static,
    {
        self.resolved_tool.downcast_ref::<T>()
    }
}

const TOOL_CALL_PENDING: u8 = 0;
const TOOL_CALL_COMPLETING: u8 = 1;
const TOOL_CALL_COMPLETED: u8 = 2;
const TOOL_CALL_ABORTING: u8 = 3;
const TOOL_CALL_ABORTED: u8 = 4;

/// Host-only execution context for one `$.tool.call`. It is constructed when
/// Rust accepts the worker API request and reused across every middleware
/// `next` run; none of its fields are decoded from Mod JSON.
#[derive(Clone)]
pub struct ModToolCallContext {
    pub transaction_id: ModToolCallTransactionId,
    /// The single Native-shaped virtual tool-use id shared by every middleware
    /// `next` run in this API transaction.
    pub virtual_tool_use_id: String,
    /// UUID of the single virtual assistant message enclosing that tool use.
    pub virtual_assistant_uuid: String,
    pub cancellation: lingxi_core::host::CancellationToken,
    pub prepared: Arc<PreparedModToolCall>,
    pub agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    finalization: Arc<AtomicU8>,
}

impl ModToolCallContext {
    fn new(
        cancellation: lingxi_core::host::CancellationToken,
        prepared: PreparedModToolCall,
        api_context: &ResolvedModApiContext,
    ) -> Self {
        let transaction_id = NEXT_MOD_TOOL_CALL_TRANSACTION_ID
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .expect("Mod tool-call transaction id space exhausted");
        let tool_use = lingxi_core::types::ToolUseId::new();
        let tool_use_suffix = tool_use
            .as_str()
            .strip_prefix("toolu_")
            .expect("fresh ToolUseId uses the toolu_ prefix");
        Self {
            transaction_id: ModToolCallTransactionId(transaction_id),
            virtual_tool_use_id: format!("toolu_plugin_{tool_use_suffix}"),
            virtual_assistant_uuid: lingxi_core::types::MessageId::new().as_uuid().to_string(),
            cancellation,
            prepared: Arc::new(prepared),
            agent_spawn_provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance {
                hook_caller: lingxi_core::host::task_registry::FieldPresence::Value(Value::String(
                    api_context.caller.plugin.clone(),
                )),
                hook_origin: native_tool_call_origin(
                    api_context.hook_origin.clone(),
                    &api_context.caller.plugin,
                ),
            },
            finalization: Arc::new(AtomicU8::new(TOOL_CALL_PENDING)),
        }
    }

    fn begin_completion(&self) -> bool {
        self.finalization
            .compare_exchange(
                TOOL_CALL_PENDING,
                TOOL_CALL_COMPLETING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish_completion(&self) {
        self.finalization
            .store(TOOL_CALL_COMPLETED, Ordering::Release);
    }

    fn release_failed_completion(&self) {
        let _ = self.finalization.compare_exchange(
            TOOL_CALL_COMPLETING,
            TOOL_CALL_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn begin_abort(&self) -> bool {
        self.finalization
            .compare_exchange(
                TOOL_CALL_PENDING,
                TOOL_CALL_ABORTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish_abort(&self) {
        self.finalization
            .store(TOOL_CALL_ABORTED, Ordering::Release);
    }

    fn is_finalized(&self) -> bool {
        matches!(
            self.finalization.load(Ordering::Acquire),
            TOOL_CALL_COMPLETED | TOOL_CALL_ABORTED
        )
    }
}

struct CancelToolCallOnDrop(Option<ModToolCallContext>);

impl CancelToolCallOnDrop {
    fn new(context: Option<&ModToolCallContext>) -> Option<Self> {
        context.cloned().map(|context| Self(Some(context)))
    }
}

impl Drop for CancelToolCallOnDrop {
    fn drop(&mut self) {
        if let Some(context) = self.0.take() {
            if !context.is_finalized() {
                context.cancellation.cancel();
            }
        }
    }
}

#[derive(Clone)]
struct ApiCallControl {
    inner: Arc<ApiCallControlInner>,
}

struct ApiCallControlInner {
    abort: AbortHandle,
    cancellation: lingxi_core::host::CancellationToken,
    cooperative: bool,
    active: AtomicBool,
}

impl ApiCallControl {
    fn new(
        abort: AbortHandle,
        cancellation: lingxi_core::host::CancellationToken,
        cooperative: bool,
    ) -> Self {
        Self {
            inner: Arc::new(ApiCallControlInner {
                abort,
                cancellation,
                cooperative,
                active: AtomicBool::new(true),
            }),
        }
    }

    fn finish(&self) {
        self.inner.active.store(false, Ordering::Release);
    }

    fn cancelled_tool_call(&self) -> bool {
        self.inner.cooperative && self.inner.cancellation.is_cancelled()
    }

    fn cancel(&self) {
        if self.inner.active.swap(false, Ordering::AcqRel) {
            self.inner.cancellation.cancel();
            if self.inner.cooperative {
                let abort = self.inner.abort.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        abort.abort();
                    });
                } else {
                    self.inner.abort.abort();
                }
            } else {
                self.inner.abort.abort();
            }
        }
    }
}

impl Drop for ApiCallControl {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Drop for ApiCallControlInner {
    fn drop(&mut self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.cancellation.cancel();
            if self.cooperative {
                let abort = self.abort.clone();
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    runtime.spawn(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        abort.abort();
                    });
                } else {
                    self.abort.abort();
                }
            } else {
                self.abort.abort();
            }
        }
    }
}

struct ReleaseToolCallCompletionOnDrop(Option<ModToolCallContext>);

impl Drop for ReleaseToolCallCompletionOnDrop {
    fn drop(&mut self) {
        if let Some(context) = self.0.take() {
            context.release_failed_completion();
            context.cancellation.cancel();
        }
    }
}

async fn complete_tool_call(
    session: &dyn ModSessionContext,
    context: &ModToolCallContext,
) -> Result<(), ModError> {
    if !context.begin_completion() {
        return Err(ModError::Unavailable(
            "$.tool.call transaction was already cancelled or finalized".into(),
        ));
    }
    let mut completion_guard = ReleaseToolCallCompletionOnDrop(Some(context.clone()));
    session.complete_tool_call(context.transaction_id).await?;
    context.finish_completion();
    completion_guard.0.take();
    Ok(())
}

async fn abort_tool_call(
    session: &dyn ModSessionContext,
    context: &ModToolCallContext,
) -> Result<(), ModError> {
    context.cancellation.cancel();
    if !context.begin_abort() {
        return Ok(());
    }
    let result = session.abort_tool_call(context.transaction_id).await;
    context.finish_abort();
    result
}

async fn acquire_tool_call_context(
    session: Option<&dyn ModSessionContext>,
    cancellation: lingxi_core::host::CancellationToken,
    input: &Utf16JsonProjection,
    api_context: &ResolvedModApiContext,
) -> Result<ModToolCallContext, ModError> {
    let plugin = api_context.caller.plugin.as_str();
    let input_view = input
        .at_pointer("")
        .filter(|view| view.object_entries().is_some())
        .ok_or_else(|| {
            native_tool_call_api_error(ModError::Hook(format!(
                "{plugin}: $.tool.call: input must be an object"
            )))
        })?;
    let requested_tool_name = input_view
        .get_property("tool")
        .and_then(|value| value.code_units())
        .and_then(|units| String::from_utf16(units).ok())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            native_tool_call_api_error(ModError::Hook(format!(
                "{plugin}: $.tool.call takes the event's input: {{ tool, ...args }}"
            )))
        })?;
    let consent = match input_view.get_property("consent") {
        None => None,
        Some(value) => match value
            .code_units()
            .and_then(|units| String::from_utf16(units).ok())
        {
            Some(consent) => Some(consent),
            None => {
                return Err(native_tool_call_api_error(ModError::Hook(format!(
                    "{plugin}: $.tool.call: consent, when given, is a string"
                ))))
            }
        },
    };
    let session = session
        .ok_or_else(|| ModError::Unavailable(format!("{plugin}: $.tool.call needs a session")))
        .map_err(native_tool_call_api_error)?;
    let prepared = session
        .prepare_tool_call(plugin, requested_tool_name, consent, cancellation.clone())
        .await
        .map_err(native_tool_call_api_error)?;
    Ok(ModToolCallContext::new(cancellation, prepared, api_context))
}

fn native_tool_call_api_error(error: ModError) -> ModError {
    match error {
        ModError::Hook(message) | ModError::Unavailable(message) => ModError::Native(message),
        other => other,
    }
}

fn new_api_origin_contexts() -> ModApiOriginContexts {
    let contexts = ModApiOriginContexts::default();
    contexts
        .register_storage(
            SEC_DEFAULT_PLUGIN,
            SEC_DEFAULT_STORAGE_ID,
            [(SEC_DEFAULT_HOOK_ID, SEC_DEFAULT_CALLER_EVENT.to_owned())],
        )
        .expect("the host-owned sec-default API caller is a valid unique registration");
    contexts
}

fn tool_call_cancelled_without_result(
    control: Option<&ApiCallControl>,
    result: &Option<Result<HostApiValue, ModError>>,
) -> bool {
    control.is_some_and(ApiCallControl::cancelled_tool_call) && !matches!(result, Some(Ok(_)))
}

fn valid_tool_call_api_projection(result: &Utf16JsonProjection) -> bool {
    if result
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .is_none()
    {
        return false;
    }
    if result.at_pointer("/deny").is_some() {
        return result.string_units("/deny").is_some() && result.at_pointer("/result").is_none();
    }
    result.at_pointer("/result").is_some()
        || (result
            .at_pointer("/isError")
            .and_then(JsonValueView::as_bool)
            == Some(true)
            && result.string_units("/text").is_some())
}

fn valid_tool_call_middleware_answer(result: &Utf16JsonProjection) -> bool {
    if result
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .is_none()
    {
        return false;
    }
    if result.at_pointer("/deny").is_some() {
        return result.string_units("/deny").is_some() && result.at_pointer("/result").is_none();
    }
    result.at_pointer("/result").is_some()
}

fn parse_mod_api_callers(
    reply: &Utf16JsonProjection,
    hooks: Option<&[String]>,
) -> Result<Vec<(u64, String)>, ModError> {
    let caller_count = reply.array_len("/apiCallers").ok_or_else(|| {
        ModError::Protocol("Mod load reply missing API caller registrations".into())
    })?;
    let mut parsed = Vec::with_capacity(caller_count);
    let mut seen = HashSet::with_capacity(caller_count);
    for index in 0..caller_count {
        let base = format!("/apiCallers/{index}");
        let hook_id = projection_u64(reply, &format!("{base}/hookId"))
            .ok_or_else(|| ModError::Protocol("Mod API caller has invalid hook id".into()))?;
        let event = projection_string(reply, &format!("{base}/event"))
            .filter(|event| !event.is_empty())
            .ok_or_else(|| ModError::Protocol("Mod API caller has invalid event".into()))?;
        if !seen.insert(hook_id)
            || hooks.is_some_and(|hooks| !hooks.iter().any(|registered| registered == &event))
        {
            return Err(ModError::Protocol(
                "Mod API caller registration does not match loaded hooks".into(),
            ));
        }
        parsed.push((hook_id, event));
    }
    Ok(parsed)
}

/// Session values are read at each Mod API call, including after `next(e)` or
/// another awaited operation changes the running session.
#[async_trait::async_trait]
pub trait ModSessionContext: Send + Sync {
    fn cwd(&self) -> PathBuf;
    fn root(&self) -> PathBuf;
    /// Trusted Projects-session observations captured by the host. Missing
    /// sources stay `None`; callers must not infer this from a working dir or
    /// worker JSON.
    async fn projects_consent_facts(&self) -> ProjectsConsentFacts {
        ProjectsConsentFacts::default()
    }
    fn surfaces(&self) -> Vec<String> {
        Vec::new()
    }
    /// Return an owned session view for delayed UI invalidation work created by
    /// this dispatch. Generation-bound sessions use it to carry cancellation
    /// across the worker callback's lifetime; ordinary sessions keep the
    /// host's attached background context.
    fn ui_invalidation_context(&self) -> Option<Arc<dyn ModSessionContext>> {
        None
    }
    /// Cancellation for the W1 generation that created this dispatch session.
    /// Background APIs spawned by `clock.after/every` use it to keep their
    /// session ticket bound to the originating executor until reset/drop.
    fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
        None
    }
    /// The fullscreen transcript selection, if this host has one.
    async fn ui_selection(&self) -> Result<Option<Utf16JsonProjection>, ModError> {
        Ok(None)
    }
    /// Forward operations that require an attached desktop/mobile UI surface.
    /// Each request and response stays in the source-backed JSON carrier.
    async fn remote_ui_operation(
        &self,
        event: &str,
        _input: Utf16JsonProjection,
        _plugin: &str,
    ) -> Result<Utf16JsonProjection, ModError> {
        match event {
            "ui.selection" => Ok(self
                .ui_selection()
                .await?
                .unwrap_or_else(|| static_projection("null"))),
            "ui.copy" => Ok(static_projection(
                r#"{"isCopied":false,"reason":"no-surface"}"#,
            )),
            "prompt.read" => Ok(static_projection(r#"{"text":"","cursor":0}"#)),
            "prompt.fill" => Ok(static_projection(
                r#"{"isFilled":false,"refusal":"no_composer","text":"","cursor":0}"#,
            )),
            "prompt.suggest" => Ok(static_projection(r#"{"isShown":false}"#)),
            _ => Err(ModError::Protocol(format!(
                "Unknown remote UI operation: {event}"
            ))),
        }
    }
    /// Forward a host-local Client VM frame to the owner of this session.
    async fn emit_mod_ui_client_frame(&self, _runtime_id: &str, _frame: &Utf16JsonProjection) {}
    /// Invalidate only the supplied UI render sites in this session.
    async fn emit_mod_ui_invalidate(
        &self,
        _instances: Option<&Utf16JsonProjection>,
        _uuid: &str,
        _session_id: &str,
    ) {
    }
    async fn messages(
        &self,
        _input: Option<Utf16JsonProjection>,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "session.messages needs a session".into(),
        ))
    }
    /// The native worker can answer this without a bound conversation by
    /// reading process-level cost/model state. This trait has no such global
    /// source, so an unbound host reports the missing session explicitly.
    async fn usage(
        &self,
        _input: Option<Utf16JsonProjection>,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "session.usage needs a session".into(),
        ))
    }
    async fn model_fork(
        &self,
        _input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Ok(static_projection(
            r#"{"isAnswered":false,"reason":"nothing-to-fork"}"#,
        ))
    }
    async fn model_complete(
        &self,
        _input: Utf16JsonProjection,
        _plugin: &str,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "model.complete needs a model client".into(),
        ))
    }
    async fn model_classify(
        &self,
        _input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _plugin: &str,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "model.classify needs a model client".into(),
        ))
    }
    async fn fs_ancestors(
        &self,
        _input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable("fs.ancestors needs a session".into()))
    }
    /// Resolve ancestor files from a child agent's effective cwd. Ordinary
    /// sessions use their existing implementation; hosts with per-agent cwd
    /// override this so `$.fs.ancestors` follows the dispatching child.
    async fn fs_ancestors_at(
        &self,
        input: Utf16JsonProjection,
        _cwd: &Path,
    ) -> Result<Utf16JsonProjection, ModError> {
        self.fs_ancestors(input).await
    }
    async fn settings_read(
        &self,
        _input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "settings.read needs a settings-aware session".into(),
        ))
    }
    async fn invalidate_prompt_section(&self) -> Result<(), ModError> {
        Err(ModError::Unavailable(
            "ui.invalidate(prompt.section) needs a session".into(),
        ))
    }
    async fn invalidate_prompt_context(&self) -> Result<(), ModError> {
        Err(ModError::Unavailable(
            "ui.invalidate(prompt.context) needs a session".into(),
        ))
    }
    async fn invalidate_prompt_attachment(&self) -> Result<(), ModError> {
        Err(ModError::Unavailable(
            "ui.invalidate(prompt.attachment) needs a session".into(),
        ))
    }
    /// Shared invalidation epoch for attachment answers cached by child agents.
    async fn prompt_attachment_generation(&self) -> u64 {
        0
    }
    async fn invalidate_tool_describe(&self) -> Result<(), ModError> {
        Err(ModError::Unavailable(
            "ui.invalidate(tool.describe) needs a session".into(),
        ))
    }
    async fn invalidate_command_describe(&self) -> Result<(), ModError> {
        Err(ModError::Unavailable(
            "ui.invalidate(command.describe) needs a session".into(),
        ))
    }
    async fn prompt_compose_facts(
        &self,
        _input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "prompt.compose needs a prompt-aware session".into(),
        ))
    }
    async fn prompt_compose_core(
        &self,
        _facts: Utf16JsonProjection,
        _origin: Option<Value>,
        _skip_hook_id: Option<u64>,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "prompt.compose needs a prompt-aware session".into(),
        ))
    }
    async fn tool_list(&self) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "tool.list needs a tool-aware session".into(),
        ))
    }
    /// Return the session's live Native `$.agent.list()` projection. This is
    /// distinct from the static registered agent-definition catalog.
    async fn agent_list(&self) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "agent.list needs a session agent registry".into(),
        ))
    }
    /// Launch one direct `$.agent.spawn()` request through the host's canonical
    /// launch-only Agent path. The input and provenance were validated by
    /// Hooks; the Host owns actual task launch and settlement.
    async fn agent_spawn_api(
        &self,
        _input: ModAgentSpawnInput,
        _context: ModAgentSpawnContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "agent.spawn needs a launch-capable session".into(),
        ))
    }
    /// Resolve and admit a direct `$.tool.call` before the worker dispatches
    /// the `tool.call` middleware event. Implementations return the canonical
    /// identity and a retained host-only resolver handle; no tool is executed
    /// until a middleware `next` reaches [`Self::tool_call`].
    async fn prepare_tool_call(
        &self,
        plugin: &str,
        _requested_tool_name: String,
        _consent: Option<String>,
        _cancellation: lingxi_core::host::CancellationToken,
    ) -> Result<PreparedModToolCall, ModError> {
        Err(ModError::Unavailable(format!(
            "{plugin}: $.tool.call needs a session tool catalog"
        )))
    }
    async fn tool_call(
        &self,
        plugin: &str,
        _input: Utf16JsonProjection,
        _context: &ModToolCallContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(format!(
            "{plugin}: $.tool.call needs a permission-aware session"
        )))
    }
    /// Projects a validated middleware answer into the direct `$.tool.call`
    /// API result. This runs after the hook-only `{result|deny}` validation,
    /// so Native API errors such as `{text, isError:true}` do not weaken the
    /// middleware answer contract.
    async fn project_tool_call_api_result(
        &self,
        _input: Utf16JsonProjection,
        accepted_answer: Utf16JsonProjection,
        _context: &ModToolCallContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        Ok(accepted_answer)
    }
    /// Called once after the outer `tool.call` middleware has returned a valid
    /// answer. Tool effects have already happened at their core call sites;
    /// this callback only closes transaction-local resources.
    async fn complete_tool_call(
        &self,
        _transaction_id: ModToolCallTransactionId,
    ) -> Result<(), ModError> {
        Ok(())
    }
    /// Called once when the outer `tool.call` fails or is cancelled. The
    /// shared token is cancelled before this callback; this only cleans up
    /// transaction-local resources and does not roll back completed tools.
    async fn abort_tool_call(
        &self,
        _transaction_id: ModToolCallTransactionId,
    ) -> Result<(), ModError> {
        Ok(())
    }
    async fn command_list(&self) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "command.list needs a command-aware session".into(),
        ))
    }
    async fn command_register(
        &self,
        _plugin: &str,
        _spec: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "command.register needs a command-aware session".into(),
        ))
    }
    async fn command_run(
        &self,
        _plugin: &str,
        _command: Utf16JsonProjection,
        _args: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "command.run needs a command queue".into(),
        ))
    }
    async fn prompt_submit(
        &self,
        _plugin: &str,
        _text: Utf16JsonProjection,
        _as_user: bool,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "prompt.submit needs a prompt queue".into(),
        ))
    }
    async fn command_unregister_plugin(&self, _plugin: &str) {}
    async fn tool_register(
        &self,
        _plugin: &str,
        _spec: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "tool.register needs a tool-aware session".into(),
        ))
    }
    fn tool_unregister_plugin(&self, _plugin: &str) {}
    async fn model(&self) -> String;
    async fn id(&self) -> String;
    async fn turns(&self) -> u64;
    async fn version(&self) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "session.version needs a versioned host".into(),
        ))
    }
    /// Query the live permission policy and tool-owned check without running
    /// PreToolUse, a classifier, a permission dialog, or the tool itself.
    async fn tool_check(
        &self,
        _tool: &str,
        _input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        Err(ModError::Unavailable(
            "tool.check needs a permission-aware session".into(),
        ))
    }

    async fn emit_mod_log(&self, _plugin: &str, _text: Utf16JsonProjection) {}
    async fn emit_mod_toast(&self, _plugin: &str, _text: Utf16JsonProjection, _timeout_ms: u64) {}
    async fn emit_mod_notify(&self, _plugin: &str, _text: Utf16JsonProjection) {}
    async fn emit_mod_status(&self, _plugin: &str, _text: Option<Utf16JsonProjection>) {}
}

/// Host-owned settings view used by `$.settings.read`. The desktop host keeps
/// source scope and managed policy discovery outside the Mod worker.
#[async_trait::async_trait]
pub trait ModSettingsReader: Send + Sync {
    async fn read(&self, input: Utf16JsonProjection) -> Result<Utf16JsonProjection, ModError>;

    /// Evaluate a resolved model against the current managed model policy.
    /// `None` means this host has no model restriction source.
    async fn model_allowed(&self, _model: &str) -> Result<Option<bool>, ModError> {
        Ok(None)
    }
}

/// Host-owned slash-command view used by `$.command.list`.
#[async_trait::async_trait]
pub trait ModCommandCatalog: Send + Sync {
    async fn list(&self) -> Result<Utf16JsonProjection, ModError>;
    async fn register(
        &self,
        plugin: &str,
        spec: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError>;
    async fn run(
        &self,
        plugin: &str,
        command: Utf16JsonProjection,
        args: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError>;
    async fn submit_prompt(
        &self,
        plugin: &str,
        text: Utf16JsonProjection,
        as_user: bool,
    ) -> Result<Utf16JsonProjection, ModError> {
        let _ = (plugin, text, as_user);
        Err(ModError::Unavailable(
            "prompt.submit needs a prompt queue".into(),
        ))
    }
    async fn unregister_plugin(&self, plugin: &str);
}

struct WeakModSessionContext(Weak<dyn ModSessionContext>);

#[async_trait::async_trait]
impl ModSessionContext for WeakModSessionContext {
    fn ui_invalidation_context(&self) -> Option<Arc<dyn ModSessionContext>> {
        self.0
            .upgrade()
            .and_then(|session| session.ui_invalidation_context())
    }

    fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
        self.0
            .upgrade()
            .and_then(|session| session.generation_cancellation_token())
    }

    fn cwd(&self) -> PathBuf {
        self.0
            .upgrade()
            .map_or_else(PathBuf::new, |session| session.cwd())
    }

    fn root(&self) -> PathBuf {
        self.0
            .upgrade()
            .map_or_else(PathBuf::new, |session| session.root())
    }

    async fn projects_consent_facts(&self) -> ProjectsConsentFacts {
        match self.0.upgrade() {
            Some(session) => session.projects_consent_facts().await,
            None => ProjectsConsentFacts::default(),
        }
    }

    fn surfaces(&self) -> Vec<String> {
        self.0
            .upgrade()
            .map_or_else(Vec::new, |session| session.surfaces())
    }

    async fn ui_selection(&self) -> Result<Option<Utf16JsonProjection>, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.ui_selection().await
    }

    async fn remote_ui_operation(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        plugin: &str,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.remote_ui_operation(event, input, plugin).await
    }

    async fn emit_mod_ui_client_frame(&self, runtime_id: &str, frame: &Utf16JsonProjection) {
        if let Some(session) = self.0.upgrade() {
            session.emit_mod_ui_client_frame(runtime_id, frame).await;
        }
    }

    async fn emit_mod_ui_invalidate(
        &self,
        instances: Option<&Utf16JsonProjection>,
        uuid: &str,
        session_id: &str,
    ) {
        if let Some(session) = self.0.upgrade() {
            session
                .emit_mod_ui_invalidate(instances, uuid, session_id)
                .await;
        }
    }

    async fn messages(
        &self,
        input: Option<Utf16JsonProjection>,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.messages(input).await
    }

    async fn usage(
        &self,
        input: Option<Utf16JsonProjection>,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.usage(input).await
    }

    async fn model_fork(
        &self,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.model_fork(input).await
    }

    async fn model_complete(
        &self,
        input: Utf16JsonProjection,
        plugin: &str,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.model_complete(input, plugin).await
    }

    async fn model_classify(
        &self,
        input: Utf16JsonProjection,
        plugin: &str,
    ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.model_classify(input, plugin).await
    }

    async fn fs_ancestors(
        &self,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.fs_ancestors(input).await
    }

    async fn fs_ancestors_at(
        &self,
        input: Utf16JsonProjection,
        cwd: &Path,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.fs_ancestors_at(input, cwd).await
    }

    async fn settings_read(
        &self,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.settings_read(input).await
    }

    async fn prompt_compose_facts(
        &self,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.prompt_compose_facts(input).await
    }

    async fn prompt_compose_core(
        &self,
        facts: Utf16JsonProjection,
        origin: Option<Value>,
        skip_hook_id: Option<u64>,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session
            .prompt_compose_core(facts, origin, skip_hook_id)
            .await
    }

    async fn invalidate_prompt_section(&self) -> Result<(), ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.invalidate_prompt_section().await
    }

    async fn invalidate_prompt_context(&self) -> Result<(), ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.invalidate_prompt_context().await
    }

    async fn invalidate_prompt_attachment(&self) -> Result<(), ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.invalidate_prompt_attachment().await
    }

    async fn prompt_attachment_generation(&self) -> u64 {
        match self.0.upgrade() {
            Some(session) => session.prompt_attachment_generation().await,
            None => 0,
        }
    }

    async fn invalidate_tool_describe(&self) -> Result<(), ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.invalidate_tool_describe().await
    }

    async fn invalidate_command_describe(&self) -> Result<(), ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.invalidate_command_describe().await
    }

    async fn tool_list(&self) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.tool_list().await
    }

    async fn agent_list(&self) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.agent_list().await
    }

    async fn agent_spawn_api(
        &self,
        input: ModAgentSpawnInput,
        context: ModAgentSpawnContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.agent_spawn_api(input, context).await
    }

    async fn prepare_tool_call(
        &self,
        plugin: &str,
        requested_tool_name: String,
        consent: Option<String>,
        cancellation: lingxi_core::host::CancellationToken,
    ) -> Result<PreparedModToolCall, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session
            .prepare_tool_call(plugin, requested_tool_name, consent, cancellation)
            .await
    }

    async fn tool_call(
        &self,
        plugin: &str,
        input: Utf16JsonProjection,
        context: &ModToolCallContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.tool_call(plugin, input, context).await
    }

    async fn project_tool_call_api_result(
        &self,
        input: Utf16JsonProjection,
        accepted_answer: Utf16JsonProjection,
        context: &ModToolCallContext,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session
            .project_tool_call_api_result(input, accepted_answer, context)
            .await
    }

    async fn complete_tool_call(
        &self,
        transaction_id: ModToolCallTransactionId,
    ) -> Result<(), ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.complete_tool_call(transaction_id).await
    }

    async fn abort_tool_call(
        &self,
        transaction_id: ModToolCallTransactionId,
    ) -> Result<(), ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.abort_tool_call(transaction_id).await
    }

    async fn command_list(&self) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.command_list().await
    }

    async fn command_register(
        &self,
        plugin: &str,
        spec: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.command_register(plugin, spec).await
    }

    async fn command_run(
        &self,
        plugin: &str,
        command: Utf16JsonProjection,
        args: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.command_run(plugin, command, args).await
    }

    async fn prompt_submit(
        &self,
        plugin: &str,
        text: Utf16JsonProjection,
        as_user: bool,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.prompt_submit(plugin, text, as_user).await
    }

    async fn command_unregister_plugin(&self, plugin: &str) {
        if let Some(session) = self.0.upgrade() {
            session.command_unregister_plugin(plugin).await;
        }
    }

    async fn tool_register(
        &self,
        plugin: &str,
        spec: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.tool_register(plugin, spec).await
    }

    fn tool_unregister_plugin(&self, plugin: &str) {
        if let Some(session) = self.0.upgrade() {
            session.tool_unregister_plugin(plugin);
        }
    }

    async fn model(&self) -> String {
        match self.0.upgrade() {
            Some(session) => session.model().await,
            None => String::new(),
        }
    }

    async fn id(&self) -> String {
        match self.0.upgrade() {
            Some(session) => session.id().await,
            None => String::new(),
        }
    }

    async fn turns(&self) -> u64 {
        match self.0.upgrade() {
            Some(session) => session.turns().await,
            None => 0,
        }
    }

    async fn version(&self) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.version().await
    }

    async fn tool_check(
        &self,
        tool: &str,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self
            .0
            .upgrade()
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into()))?;
        session.tool_check(tool, input).await
    }

    async fn emit_mod_log(&self, plugin: &str, text: Utf16JsonProjection) {
        if let Some(session) = self.0.upgrade() {
            session.emit_mod_log(plugin, text).await;
        }
    }

    async fn emit_mod_toast(&self, plugin: &str, text: Utf16JsonProjection, timeout_ms: u64) {
        if let Some(session) = self.0.upgrade() {
            session.emit_mod_toast(plugin, text, timeout_ms).await;
        }
    }

    async fn emit_mod_notify(&self, plugin: &str, text: Utf16JsonProjection) {
        if let Some(session) = self.0.upgrade() {
            session.emit_mod_notify(plugin, text).await;
        }
    }

    async fn emit_mod_status(&self, plugin: &str, text: Option<Utf16JsonProjection>) {
        if let Some(session) = self.0.upgrade() {
            session.emit_mod_status(plugin, text).await;
        }
    }
}

fn resolve_mod_api_path(raw: &str, cwd: &Path) -> Result<PathBuf, ModError> {
    if raw.starts_with("//") || raw.starts_with("\\\\") {
        return Err(ModError::Hook(
            "network file paths are not supported".into(),
        ));
    }
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

fn valid_tool_check_result(result: &Utf16JsonProjection) -> bool {
    let Some(value) = result.at_pointer("") else {
        return false;
    };
    value.object_entries().is_some()
        && matches!(
            result.string_at("/decision"),
            Some("allow" | "ask" | "deny")
        )
        && ["reason", "rule"].iter().all(|key| {
            result
                .at_pointer(&format!("/{key}"))
                .is_none_or(|value| value.as_str().is_some())
        })
}

fn valid_tool_list_result(result: &Utf16JsonProjection) -> bool {
    let Some(count) = result.array_len("") else {
        return false;
    };
    (0..count).all(|index| {
        let prefix = format!("/{index}");
        result.string_at(&format!("{prefix}/name")).is_some()
            && result.string_at(&format!("{prefix}/description")).is_some()
            && result
                .at_pointer(&format!("{prefix}/mcp"))
                .and_then(JsonValueView::as_bool)
                .is_some()
    })
}

fn valid_model_fork_result(result: &Utf16JsonProjection) -> bool {
    let Some(answered) = result
        .at_pointer("/isAnswered")
        .and_then(JsonValueView::as_bool)
    else {
        return false;
    };
    if answered {
        if result.string_at("/text").is_none() {
            return false;
        }
    } else {
        let Some(reason) = result.string_at("/reason") else {
            return false;
        };
        if reason == "nothing-to-fork" && result.at_pointer("/usage").is_none() {
            return true;
        }
    }
    [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .all(|key| projection_u64(result, &format!("/usage/{key}")).is_some())
}

fn valid_session_usage_result(result: &Utf16JsonProjection) -> bool {
    let Some(fields) = result
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
    else {
        return false;
    };
    if fields.len() != 4
        || !["startedAt", "context", "rateLimits", "cost"]
            .iter()
            .all(|key| result.at_pointer(&format!("/{key}")).is_some())
    {
        return false;
    }
    if result.string_at("/startedAt").is_none() {
        return false;
    }

    let Some(context) = result.at_pointer("/context") else {
        return false;
    };
    let Some(context_fields) = context.object_entries() else {
        return false;
    };
    if !context_fields
        .iter()
        .any(|field| field.display_key == "window")
        || context_fields.iter().any(|field| {
            !matches!(
                field.display_key,
                "window" | "tokens" | "percent" | "breakdown"
            )
        })
        || !context
            .get_property("window")
            .and_then(JsonValueView::as_number)
            .is_some_and(|window| window.is_finite() && window >= 0.0)
    {
        return false;
    }
    let has_tokens = context.get_property("tokens").is_some();
    if has_tokens != context.get_property("percent").is_some()
        || (has_tokens
            && !context
                .get_property("tokens")
                .and_then(|_| projection_u64(result, "/context/tokens"))
                .is_some_and(|tokens| tokens > 0))
        || (context.get_property("percent").is_some()
            && !context
                .get_property("percent")
                .and_then(JsonValueView::as_number)
                .is_some_and(|percent| percent.is_finite() && (0.0..=100.0).contains(&percent)))
        || (context
            .get_property("breakdown")
            .is_some_and(|value| value.object_entries().is_none()))
    {
        return false;
    }

    let Some(limits_count) = result.array_len("/rateLimits") else {
        return false;
    };
    if !(0..limits_count).all(|index| {
        let pointer = format!("/rateLimits/{index}");
        let Some(limit) = result.at_pointer(&pointer) else {
            return false;
        };
        limit
            .object_entries()
            .is_some_and(|fields| fields.len() == 3)
            && matches!(
                result.string_at(&format!("{pointer}/kind")),
                Some("five_hour" | "seven_day" | "spend_limit")
            )
            && limit
                .get_property("percentUsed")
                .and_then(JsonValueView::as_number)
                .is_some_and(|percent| percent.is_finite() && (0.0..=100.0).contains(&percent))
            && result.string_at(&format!("{pointer}/resetsAt")).is_some()
    }) {
        return false;
    }

    let Some(cost) = result.at_pointer("/cost") else {
        return false;
    };
    cost.object_entries()
        .is_some_and(|fields| fields.len() == 1)
        && cost
            .get_property("usd")
            .and_then(JsonValueView::as_number)
            .is_some_and(|usd| usd.is_finite() && usd >= 0.0)
}

fn valid_model_complete_result(result: &Utf16JsonProjection) -> bool {
    result.string_at("/reason") != Some("nothing-to-fork") && valid_model_fork_result(result)
}

/// Extract an exact JSON subtree without lowering it through `serde_json::Value`.
fn mod_exact_json_at_pointer(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    pointer: &str,
) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, String> {
    if projection.at_pointer(pointer).is_none() {
        return Err(format!("Mod API {pointer} is required"));
    }
    projection
        .subprojection(pointer)
        .map_err(|error| error.to_string())
}

/// Preserve absence separately from a present JSON null while extracting an
/// exact JSON subtree from the flat Core carrier.
fn mod_optional_exact_json_at_pointer(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    pointer: &str,
) -> Result<Option<lingxi_core::types::utf16_json::Utf16JsonProjection>, String> {
    if projection.at_pointer(pointer).is_none() {
        return Ok(None);
    }
    mod_exact_json_at_pointer(projection, pointer).map(Some)
}

fn mod_json_string_units_at_pointer(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    pointer: &str,
) -> Result<Vec<u16>, String> {
    projection
        .string_units(pointer)
        .ok_or_else(|| format!("Mod API {pointer} must be a string"))
}

fn mod_json_object_fields(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<
    Vec<(
        Vec<u16>,
        lingxi_core::types::utf16_json::Utf16JsonProjection,
    )>,
    String,
> {
    let entries = projection
        .object_entries()
        .ok_or_else(|| "Mod JSON value must be an object".to_owned())?;
    entries
        .into_iter()
        .map(|entry| {
            let key = entry.code_units.to_vec();
            projection
                .subprojection_property_units(&key)
                .map(|value| (key, value))
                .map_err(|error| error.to_string())
        })
        .collect()
}

fn mod_json_object_get(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    key: &[u16],
) -> Result<Option<lingxi_core::types::utf16_json::Utf16JsonProjection>, String> {
    let entries = projection
        .object_entries()
        .ok_or_else(|| "Mod store is not a JSON object".to_owned())?;
    if !entries.iter().any(|entry| entry.code_units == key) {
        return Ok(None);
    }
    projection
        .subprojection_property_units(key)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn mod_json_object_set(
    projection: &mut lingxi_core::types::utf16_json::Utf16JsonProjection,
    key: Vec<u16>,
    value: lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<(), String> {
    let mut fields = mod_json_object_fields(projection)?;
    if let Some((_, existing)) = fields
        .iter_mut()
        .find(|(candidate, _)| candidate.as_slice() == key.as_slice())
    {
        *existing = value;
    } else {
        fields.push((key, value));
    }
    *projection = lingxi_core::types::utf16_json::Utf16JsonProjection::object_units(fields)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn mod_json_object_delete(
    projection: &mut lingxi_core::types::utf16_json::Utf16JsonProjection,
    key: &[u16],
) -> Result<bool, String> {
    let mut fields = mod_json_object_fields(projection)?;
    let original_len = fields.len();
    fields.retain(|(candidate, _)| candidate.as_slice() != key);
    if fields.len() == original_len {
        return Ok(false);
    }
    *projection = lingxi_core::types::utf16_json::Utf16JsonProjection::object_units(fields)
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn mod_json_array_index(key_units: &[u16]) -> Option<u32> {
    let key = String::from_utf16(key_units).ok()?;
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}

/// `Object.keys` orders array-index property names numerically, followed by
/// all other exact UTF-16 names in insertion order.
fn mod_json_object_keys(
    projection: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> Result<Vec<Vec<u16>>, String> {
    let entries = projection
        .object_entries()
        .ok_or_else(|| "Mod store is not a JSON object".to_owned())?;
    let mut integer_keys = Vec::new();
    let mut other_keys = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if let Some(index) = mod_json_array_index(entry.code_units) {
            integer_keys.push((index, position, entry.code_units.to_vec()));
        } else {
            other_keys.push((position, entry.code_units.to_vec()));
        }
    }
    integer_keys.sort_by_key(|(index, _, _)| *index);
    Ok(integer_keys
        .into_iter()
        .map(|(_, _, key)| key)
        .chain(other_keys.into_iter().map(|(_, key)| key))
        .collect())
}

fn mod_json_string_projection(
    code_units: Vec<u16>,
) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, String> {
    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&code_units),
        code_units,
    )
    .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModStateKey {
    plugin: Vec<u16>,
    key: Vec<u16>,
    id: Option<Vec<u16>>,
}

impl ModStateKey {
    #[cfg(test)]
    fn test(plugin: &str, key: &str) -> Self {
        Self {
            plugin: plugin.encode_utf16().collect(),
            key: key.encode_utf16().collect(),
            id: None,
        }
    }
}

fn mod_state_key(
    input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    event: &str,
) -> Result<ModStateKey, ModError> {
    let args = input.at_pointer("").ok_or_else(|| {
        ModError::Hook(format!(
            "{event} takes a reference {{ plugin, key }} (id for a family member)"
        ))
    })?;
    let entries = args.object_entries().ok_or_else(|| {
        ModError::Hook(format!(
            "{event} takes a reference {{ plugin, key }} (id for a family member)"
        ))
    })?;
    let plugin = args
        .get_property("plugin")
        .filter(|value| value.as_str().is_some())
        .ok_or_else(|| ModError::Hook(format!("{event} plugin must be a non-empty string")))?;
    let key = args
        .get_property("key")
        .filter(|value| value.as_str().is_some())
        .ok_or_else(|| ModError::Hook(format!("{event} key must be a non-empty string")))?;
    let plugin_units = plugin.code_units().unwrap_or_default().to_vec();
    let key_units = key.code_units().unwrap_or_default().to_vec();
    let id_units = args
        .get_property("id")
        .map(|value| {
            value
                .code_units()
                .map(ToOwned::to_owned)
                .ok_or_else(|| ModError::Hook(format!("{event} id must be a string")))
        })
        .transpose()?;
    if plugin_units.is_empty() {
        return Err(ModError::Hook(format!(
            "{event} plugin must be a non-empty string"
        )));
    }
    if key_units.is_empty() {
        return Err(ModError::Hook(format!(
            "{event} key must be a non-empty string"
        )));
    }
    if plugin_units.contains(&0)
        || key_units.contains(&0)
        || id_units.as_ref().is_some_and(|units| units.contains(&0))
    {
        return Err(ModError::Hook(format!(
            "{event} plugin, key and id cannot contain NUL"
        )));
    }
    let allowed: &[&str] = if event == "state.get" {
        &["plugin", "key", "id"]
    } else {
        &["plugin", "key", "id", "value", "previous", "ifVersion"]
    };
    let allowed_units = allowed
        .iter()
        .map(|field| field.encode_utf16().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    if let Some(extra) = entries
        .iter()
        .find(|entry| !allowed_units.iter().any(|field| field == entry.code_units))
    {
        let visible = String::from_utf16_lossy(extra.code_units);
        return Err(ModError::Hook(format!("{event} has extra field {visible}")));
    }
    Ok(ModStateKey {
        plugin: plugin_units,
        key: key_units,
        id: id_units,
    })
}

fn ui_render_scope_string<'a>(object: JsonValueView<'a>, field: &str) -> Result<&'a str, ModError> {
    object
        .get_property(field)
        .and_then(JsonValueView::as_str)
        .filter(|value| !value.is_empty() && !value.contains('\0'))
        .ok_or_else(|| ModError::Protocol(format!("uiRenderReadScope {field} is invalid")))
}

fn ui_render_input_is_on_screen(input: &Utf16JsonProjection) -> bool {
    let value = input
        .at_pointer("/onScreen")
        .filter(|value| !value.is_null())
        .or_else(|| input.at_pointer("/on_screen"))
        .filter(|value| !value.is_null())
        .or_else(|| input.at_pointer("/props/onScreen"));
    value.is_some_and(|value| value.as_bool().unwrap_or(!value.is_null()))
}

fn parse_ui_render_state_read_scope(
    api_message: &Utf16JsonProjection,
    method: &str,
    parent_event: &str,
    parent_input: &Utf16JsonProjection,
    host_render_revision: Option<u64>,
) -> Result<Option<ModUiRenderStateReadScope>, ModError> {
    let Some(scope) = api_message.at_pointer("/uiRenderReadScope") else {
        return Ok(None);
    };
    let entries = scope
        .object_entries()
        .ok_or_else(|| ModError::Protocol("uiRenderReadScope must be an object".into()))?;
    let carries_state_read = method == "state.get";
    let carries_render_invalidation =
        method == "ui.invalidate" && api_message.string_at("/input/event") == Some("ui.render");
    if !(carries_state_read || carries_render_invalidation)
        || parent_event != "ui.render"
        || entries.iter().any(|entry| {
            !["surface", "component", "requestId", "revision", "onScreen"]
                .contains(&entry.display_key)
        })
    {
        return Err(ModError::Protocol(
            "uiRenderReadScope is only valid for state.get or ui.render invalidation during ui.render".into(),
        ));
    }
    let surface = ui_render_scope_string(scope, "surface")?;
    let component = ui_render_scope_string(scope, "component")?;
    let request_id = scope
        .get_property("requestId")
        .and_then(JsonValueView::code_units)
        .filter(|value| !value.is_empty() && !value.contains(&0))
        .map(|value| value.to_vec())
        .ok_or_else(|| ModError::Protocol("uiRenderReadScope requestId is invalid".into()))?;
    let revision = projection_u64(api_message, "/uiRenderReadScope/revision")
        .filter(|revision| *revision > 0)
        .ok_or_else(|| ModError::Protocol("uiRenderReadScope revision is invalid".into()))?;
    let on_screen = scope
        .get_property("onScreen")
        .and_then(JsonValueView::as_bool)
        .ok_or_else(|| ModError::Protocol("uiRenderReadScope onScreen is invalid".into()))?;
    if surface != parent_input.string_at("/surface").unwrap_or_default()
        || component != parent_input.string_at("/component").unwrap_or_default()
        || parent_input.string_units("/requestId").as_deref() != Some(request_id.as_slice())
        || on_screen != ui_render_input_is_on_screen(parent_input)
        || host_render_revision != Some(revision)
    {
        return Err(ModError::Protocol(
            "uiRenderReadScope does not match its parent ui.render dispatch".into(),
        ));
    }
    Ok(Some(ModUiRenderStateReadScope {
        site: ModUiRenderSiteKey {
            surface: surface.to_owned(),
            component: component.to_owned(),
            request_id,
        },
        revision,
        on_screen,
    }))
}

fn valid_prompt_context_result(result: &Utf16JsonProjection) -> bool {
    let Some(blocks) = result.at_pointer("/blocks") else {
        return false;
    };
    let Some(block_count) = blocks.array_len() else {
        return false;
    };
    if block_count > 32 {
        return false;
    }
    for index in 0..block_count {
        let Some(name) = result.string_units(&format!("/blocks/{index}/name")) else {
            return false;
        };
        if name.is_empty()
            || result
                .at_pointer(&format!("/blocks/{index}/text"))
                .and_then(JsonValueView::code_units)
                .is_none()
        {
            return false;
        }
    }
    let Some(files) = result.at_pointer("/instructionFiles") else {
        return true;
    };
    let Some(file_count) = files.array_len() else {
        return false;
    };
    let mut paths = HashSet::new();
    for index in 0..file_count {
        let path_pointer = format!("/instructionFiles/{index}/path");
        let Some(path) = result.string_units(&path_pointer) else {
            return false;
        };
        let kind = result.string_at(&format!("/instructionFiles/{index}/kind"));
        if path.is_empty()
            || !paths.insert(path)
            || !matches!(
                kind,
                Some("managed" | "user" | "project" | "local" | "memory")
            )
            || result
                .at_pointer(&format!("/instructionFiles/{index}/content"))
                .and_then(JsonValueView::code_units)
                .is_none()
            || result
                .at_pointer(&format!("/instructionFiles/{index}/parent"))
                .is_some_and(|parent| parent.code_units().is_none())
        {
            return false;
        }
    }
    true
}

fn same_projection_field(
    left: &Utf16JsonProjection,
    right: &Utf16JsonProjection,
    pointer: &str,
) -> bool {
    match (left.at_pointer(pointer), right.at_pointer(pointer)) {
        (Some(_), Some(_)) => left
            .subprojection(pointer)
            .ok()
            .zip(right.subprojection(pointer).ok())
            .is_some_and(|(left, right)| left.semantically_eq(&right)),
        (None, None) => true,
        _ => false,
    }
}

fn same_projection_field_opt(
    left: &Option<Utf16JsonProjection>,
    right: &Option<Utf16JsonProjection>,
    pointer: &str,
) -> bool {
    match (left.as_ref(), right.as_ref()) {
        (Some(left), Some(right)) => same_projection_field(left, right, pointer),
        (None, None) => true,
        _ => false,
    }
}

fn valid_prompt_submit_forwarded(
    original: &Utf16JsonProjection,
    forwarded: &Utf16JsonProjection,
) -> bool {
    forwarded
        .at_pointer("/text")
        .and_then(JsonValueView::code_units)
        .is_some()
        && same_projection_field(original, forwarded, "/wait")
        && same_projection_field(original, forwarded, "/origin")
        && forwarded.at_pointer("/context").is_none_or(|context| {
            let Some(length) = context.array_len() else {
                return false;
            };
            (0..length).all(|index| {
                context
                    .get_index(index)
                    .and_then(JsonValueView::code_units)
                    .is_some_and(|units| !units.is_empty())
            })
        })
}

fn valid_prompt_compose_result(result: &Utf16JsonProjection) -> bool {
    let Some(sections) = result.at_pointer("/sections") else {
        return false;
    };
    let Some(section_count) = sections.array_len() else {
        return false;
    };
    let mut ids = HashSet::new();
    let mut in_session = false;
    for index in 0..section_count {
        let id_pointer = format!("/sections/{index}/id");
        let Some(id) = result.string_units(&id_pointer) else {
            return false;
        };
        if id.is_empty()
            || !ids.insert(id)
            || result
                .at_pointer(&format!("/sections/{index}/text"))
                .and_then(JsonValueView::code_units)
                .is_none()
        {
            return false;
        }
        match result.string_at(&format!("/sections/{index}/scope")) {
            Some("shared") => {
                if in_session {
                    return false;
                }
            }
            Some("session") => in_session = true,
            _ => return false,
        }
    }
    true
}

fn valid_command_list_result(result: &Utf16JsonProjection) -> bool {
    let Some(count) = result.array_len("") else {
        return false;
    };
    (0..count).all(|index| {
        let prefix = format!("/{index}");
        result.string_at(&format!("{prefix}/name")).is_some()
            && result.string_at(&format!("{prefix}/description")).is_some()
            && matches!(
                result.string_at(&format!("{prefix}/source")),
                Some("builtin" | "plugin" | "user" | "mcp")
            )
            && result
                .at_pointer(&format!("{prefix}/plugin"))
                .is_none_or(|value| value.as_str().is_some())
    })
}

async fn git_text(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new("git").current_dir(cwd).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn session_repo(cwd: &Path) -> Value {
    let Some(worktree_root) = git_text(cwd, &["rev-parse", "--show-toplevel"]).await else {
        return Value::Null;
    };
    let common_dir = git_text(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    .and_then(|path| std::fs::canonicalize(path).ok());
    let root = common_dir
        .as_deref()
        .filter(|dir| dir.file_name().is_some_and(|name| name == ".git"))
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new(&worktree_root));
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let remote = git_text(
        cwd,
        &["config", "--local", "--get", "remote.origin.pushurl"],
    )
    .await;
    let remote = match remote {
        Some(remote) => Some(remote),
        None => git_text(cwd, &["config", "--local", "--get", "remote.origin.url"]).await,
    };
    let remote = remote.map(|url| {
        // The 2.1.287 engine strips a URL's user-info before exposing it to a
        // Mod (`/^([a-z][a-z0-9+.-]*:\/\/)[^/]*@/i`). Scp-style remotes are
        // left as Git recorded them.
        regex::Regex::new(r"(?i)^([a-z][a-z0-9+.-]*://)[^/]*@")
            .expect("fixed remote sanitization regex")
            .replace(&url, "$1")
            .into_owned()
    });
    json!({
        "root": root.to_string_lossy(),
        "remote": remote,
        "internal": false,
        "name": null,
    })
}

fn mod_store_file(root: &Path, plugin: &str) -> PathBuf {
    let safe = !plugin.is_empty()
        && plugin.len() <= 64
        && plugin.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
        && !matches!(plugin, "." | "..")
        && !matches!(
            plugin.split('.').next().unwrap_or(""),
            "con"
                | "prn"
                | "aux"
                | "nul"
                | "com1"
                | "com2"
                | "com3"
                | "com4"
                | "com5"
                | "com6"
                | "com7"
                | "com8"
                | "com9"
                | "lpt1"
                | "lpt2"
                | "lpt3"
                | "lpt4"
                | "lpt5"
                | "lpt6"
                | "lpt7"
                | "lpt8"
                | "lpt9"
        );
    if safe {
        return root.join(format!("{plugin}.json"));
    }
    let cleaned = plugin
        .encode_utf16()
        .map(|unit| match unit {
            65..=90 | 97..=122 | 48..=57 | 45 | 95 => unit as u8 as char,
            _ => '_',
        })
        .take(64)
        .collect::<String>();
    let hash = Sha256::digest(plugin.as_bytes());
    let short_hash = hash[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    root.join(format!(
        "{}-{short_hash}.json",
        if cleaned.is_empty() {
            "plugin"
        } else {
            &cleaned
        }
    ))
}

fn js_object_keys(map: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut integer_keys = Vec::new();
    let mut other_keys = Vec::new();
    for key in map.keys() {
        let index = key
            .parse::<u32>()
            .ok()
            .filter(|index| *index != u32::MAX && index.to_string() == *key);
        if let Some(index) = index {
            integer_keys.push((index, key.clone()));
        } else {
            other_keys.push(key.clone());
        }
    }
    integer_keys.sort_by_key(|(index, _)| *index);
    integer_keys
        .into_iter()
        .map(|(_, key)| key)
        .chain(other_keys)
        .collect()
}

type Routes = Arc<StdMutex<HashMap<u64, mpsc::UnboundedSender<Utf16JsonProjection>>>>;

/// Read a protocol selector only when its UTF-16 representation is valid Rust
/// text. Dynamic payloads stay in the flat projection; this conversion is for
/// bounded routing metadata such as method names and opaque host tickets.
fn projection_string(message: &Utf16JsonProjection, pointer: &str) -> Option<String> {
    String::from_utf16(&message.string_units(pointer)?).ok()
}

/// Worker request ids are JavaScript Numbers. Reject non-integral and unsafe
/// values rather than route two distinct Rust ids through one rounded JS id.
fn projection_u64(message: &Utf16JsonProjection, pointer: &str) -> Option<u64> {
    let value = message.number_at(pointer)?;
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    (value.is_finite() && value >= 0.0 && value <= MAX_SAFE_INTEGER && value.fract() == 0.0)
        .then_some(value as u64)
}

fn projection_string_value(value: &str) -> Result<Utf16JsonProjection, ModError> {
    Utf16JsonProjection::root_string(value.to_owned(), value.encode_utf16().collect())
        .map_err(|error| ModError::Protocol(error.to_string()))
}

fn projection_u64_value(value: u64) -> Result<Utf16JsonProjection, ModError> {
    Utf16JsonProjection::parse(&value.to_string())
        .map_err(|error| ModError::Protocol(error.to_string()))
}

fn projection_object(
    fields: Vec<(String, Utf16JsonProjection)>,
) -> Result<Utf16JsonProjection, ModError> {
    Utf16JsonProjection::object(fields).map_err(|error| ModError::Protocol(error.to_string()))
}

fn projection_root_string_member_or_empty(
    message: &Utf16JsonProjection,
    pointer: &str,
) -> Result<Utf16JsonProjection, ModError> {
    match message
        .at_pointer(pointer)
        .and_then(|value| value.code_units())
    {
        Some(units) => {
            Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
                .map_err(|error| ModError::Protocol(error.to_string()))
        }
        None => projection_string_value(""),
    }
}

fn projection_optional_string_member(
    message: &Utf16JsonProjection,
    pointer: &str,
) -> Result<Option<Utf16JsonProjection>, ModError> {
    match message
        .at_pointer(pointer)
        .and_then(|value| value.code_units())
    {
        Some(units) => {
            Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
                .map(Some)
                .map_err(|error| ModError::Protocol(error.to_string()))
        }
        None => Ok(None),
    }
}

const MOD_IPC_PROTOCOL: &str = "lingxi.mods.ipc";
const MOD_IPC_VERSION: u64 = 1;

#[derive(Debug)]
struct ModExceptionalNumber {
    pointer_units: Vec<u16>,
    bits: u64,
}

fn append_json_pointer_segment(pointer: &mut Vec<u16>, segment: &[u16]) {
    pointer.push(u16::from(b'/'));
    for unit in segment {
        match *unit {
            unit if unit == u16::from(b'~') => {
                pointer.extend([u16::from(b'~'), u16::from(b'0')]);
            }
            unit if unit == u16::from(b'/') => {
                pointer.extend([u16::from(b'~'), u16::from(b'1')]);
            }
            unit => pointer.push(unit),
        }
    }
}

fn collect_mod_exceptional_numbers(
    message: &Utf16JsonProjection,
) -> Result<Vec<ModExceptionalNumber>, ModError> {
    enum Visit<'a> {
        Node(JsonValueView<'a>),
        AppendSegment(Vec<u16>),
        RestorePath(usize),
    }

    let root = message
        .at_pointer("")
        .ok_or_else(|| ModError::Protocol("Mod IPC message has no root node".into()))?;
    let mut pending = vec![Visit::Node(root)];
    let mut pointer_units = Vec::<u16>::new();
    let mut exceptional = Vec::new();

    while let Some(task) = pending.pop() {
        let value = match task {
            Visit::AppendSegment(segment) => {
                append_json_pointer_segment(&mut pointer_units, &segment);
                continue;
            }
            Visit::RestorePath(length) => {
                pointer_units.truncate(length);
                continue;
            }
            Visit::Node(value) => value,
        };

        if let Some(number) = value.as_number() {
            if !number.is_finite() || (number == 0.0 && number.is_sign_negative()) {
                exceptional.push(ModExceptionalNumber {
                    pointer_units: pointer_units.clone(),
                    bits: number.to_bits(),
                });
            }
            continue;
        }

        if let Some(entries) = value.object_entries() {
            for entry in entries.into_iter().rev() {
                pending.push(Visit::RestorePath(pointer_units.len()));
                pending.push(Visit::Node(entry.value));
                pending.push(Visit::AppendSegment(entry.code_units.to_vec()));
            }
            continue;
        }

        if let Some(length) = value.array_len() {
            for index in (0..length).rev() {
                let child = value.get_index(index).ok_or_else(|| {
                    ModError::Protocol("Mod IPC array changed while encoding".into())
                })?;
                pending.push(Visit::RestorePath(pointer_units.len()));
                pending.push(Visit::Node(child));
                pending.push(Visit::AppendSegment(
                    index.to_string().encode_utf16().collect(),
                ));
            }
        }
    }

    Ok(exceptional)
}

fn encode_mod_ipc_frame(message: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ModError> {
    let exceptional = collect_mod_exceptional_numbers(message)?;
    let mut sanitized = message.clone();
    let null = static_projection("null");
    let mut number_bits = Vec::with_capacity(exceptional.len());

    for number in exceptional {
        sanitized
            .replace_pointer_projection_units(&number.pointer_units, null.clone())
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let pointer = Utf16JsonProjection::array(
            number
                .pointer_units
                .iter()
                .map(|unit| projection_u64_value(u64::from(*unit)))
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        let bits = Utf16JsonProjection::from(format!("{:016x}", number.bits));
        number_bits.push(object_projection(vec![
            ("pointerUnits".into(), pointer),
            ("bits".into(), bits),
        ])?);
    }

    let number_bits = Utf16JsonProjection::array(number_bits)
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    object_projection(vec![
        (
            "protocol".into(),
            Utf16JsonProjection::from(MOD_IPC_PROTOCOL),
        ),
        ("version".into(), projection_u64_value(MOD_IPC_VERSION)?),
        ("message".into(), sanitized),
        ("numberBits".into(), number_bits),
    ])
}

fn parse_mod_ipc_frame(line: &str) -> Result<Utf16JsonProjection, ModError> {
    let frame =
        Utf16JsonProjection::parse(line).map_err(|error| ModError::Protocol(error.to_string()))?;
    if frame.string_at("/protocol") != Some(MOD_IPC_PROTOCOL)
        || frame.number_at("/version") != Some(MOD_IPC_VERSION as f64)
    {
        return Err(ModError::Protocol("invalid Mod IPC frame header".into()));
    }

    let mut message = frame
        .subprojection("/message")
        .map_err(|error| ModError::Protocol(error.to_string()))?;
    let count = frame
        .array_len("/numberBits")
        .ok_or_else(|| ModError::Protocol("Mod IPC frame has no number sidecar".into()))?;
    for entry_index in 0..count {
        let pointer_array = format!("/numberBits/{entry_index}/pointerUnits");
        let pointer_len = frame
            .array_len(&pointer_array)
            .ok_or_else(|| ModError::Protocol("invalid Mod IPC number pointer".into()))?;
        let mut pointer_units = Vec::with_capacity(pointer_len);
        for unit_index in 0..pointer_len {
            let unit_pointer = format!("{pointer_array}/{unit_index}");
            let unit = frame
                .number_at(&unit_pointer)
                .ok_or_else(|| ModError::Protocol("invalid Mod IPC number pointer unit".into()))?;
            if !unit.is_finite() || unit.fract() != 0.0 || !(0.0..=65535.0).contains(&unit) {
                return Err(ModError::Protocol(
                    "Mod IPC number pointer unit is out of range".into(),
                ));
            }
            pointer_units.push(unit as u16);
        }

        let bits_pointer = format!("/numberBits/{entry_index}/bits");
        let bits_text = frame
            .string_at(&bits_pointer)
            .filter(|bits| bits.len() == 16 && bits.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| ModError::Protocol("invalid Mod IPC number bits".into()))?;
        let bits = u64::from_str_radix(bits_text, 16)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let number = f64::from_bits(bits);
        if number.is_finite() && !(number == 0.0 && number.is_sign_negative()) {
            return Err(ModError::Protocol(
                "Mod IPC number sidecar contains an ordinary number".into(),
            ));
        }

        let placeholder = message
            .subprojection_units(&pointer_units)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        if !placeholder
            .at_pointer("")
            .is_some_and(JsonValueView::is_null)
        {
            return Err(ModError::Protocol(
                "Mod IPC number placeholder is not null".into(),
            ));
        }
        let mut builder = Utf16JsonBuilder::new();
        let root = builder.push_number(number);
        let replacement = builder
            .finish(root)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        message
            .replace_pointer_projection_units(&pointer_units, replacement)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
    }

    Ok(message)
}

fn parse_mod_worker_line(line: &str) -> Result<Utf16JsonProjection, ModError> {
    parse_mod_ipc_frame(line)
}

/// The manager-owned description needed to re-admit one currently loaded Mod
/// after the worker process dies. The manager supplies modules in registration
/// order so stable ties retain their original dispatch sequence.
#[derive(Debug, Clone)]
pub struct ModReplayModule {
    pub plugin: String,
    pub storage_id: String,
    pub root: PathBuf,
    pub module: PathBuf,
    pub options: Utf16JsonProjection,
    pub tier: String,
    pub tier_order: Option<u32>,
    pub version: Option<String>,
    pub provenance: String,
}

/// One worker-process death. The current protocol cannot identify a culprit
/// for EOF/pipe loss, so those failures leave `attributed_storage_id` empty.
#[derive(Debug, Clone)]
pub struct ModWorkerFailure {
    pub epoch: u64,
    pub reason: String,
    pub attributed_storage_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModReplayFailure {
    pub storage_id: String,
    pub reason: String,
}

struct ModWorkerEpoch {
    id: u64,
    child: Arc<Mutex<Child>>,
    input: Arc<Mutex<ChildStdin>>,
    worker_bootstrap: Option<Arc<ModWorkerBootstrap>>,
    routes: Routes,
    generation_api_sessions: GenerationApiSessions,
    status: AtomicU8,
    router_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    background_api_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    failure_reported: AtomicBool,
}

impl ModWorkerEpoch {
    fn new(
        id: u64,
        child: Child,
        input: ChildStdin,
        worker_bootstrap: Option<Arc<ModWorkerBootstrap>>,
        recovering: bool,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<Utf16JsonProjection>) {
        let routes = Arc::new(StdMutex::new(HashMap::new()));
        let (background_sender, background_replies) = mpsc::unbounded_channel();
        routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(0, background_sender);
        (
            Arc::new(Self {
                id,
                child: Arc::new(Mutex::new(child)),
                input: Arc::new(Mutex::new(input)),
                worker_bootstrap,
                routes,
                generation_api_sessions: Arc::new(StdMutex::new(HashMap::new())),
                status: AtomicU8::new(if recovering {
                    WORKER_EPOCH_RECOVERING
                } else {
                    WORKER_EPOCH_RUNNING
                }),
                router_task: Mutex::new(None),
                background_api_task: Mutex::new(None),
                failure_reported: AtomicBool::new(false),
            }),
            background_replies,
        )
    }
}

struct RequestChannel {
    id: u64,
    api_context_ticket: Option<String>,
    generation_context_ticket: Option<String>,
    generation_api_sessions: GenerationApiSessions,
    replies: mpsc::UnboundedReceiver<Utf16JsonProjection>,
    routes: Routes,
    input: Arc<Mutex<ChildStdin>>,
    worker_epoch: Option<Arc<ModWorkerEpoch>>,
    active_request_epochs: Option<Arc<StdMutex<HashMap<u64, Weak<ModWorkerEpoch>>>>>,
    completed: bool,
}

struct RevokeApiCallersOnDrop {
    contexts: ModApiOriginContexts,
    storage_id: String,
    armed: bool,
}

impl RevokeApiCallersOnDrop {
    fn new(contexts: ModApiOriginContexts, storage_id: &str) -> Self {
        Self {
            contexts,
            storage_id: storage_id.to_owned(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RevokeApiCallersOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.contexts.unregister_storage(&self.storage_id);
        }
    }
}

impl Drop for RequestChannel {
    fn drop(&mut self) {
        self.routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        if let Some(ticket) = self.generation_context_ticket.take() {
            release_generation_api_session(&self.generation_api_sessions, &ticket);
        }
        if let Some(epochs) = self.active_request_epochs.take() {
            epochs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.id);
        }
        self.worker_epoch.take();
        if !self.completed {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let input = self.input.clone();
                let id = self.id;
                runtime.spawn(async move {
                    let line = generated_projection(&json!({"id":id,"kind":"cancel"}))
                        .and_then(|message| encode_mod_ipc_frame(&message))
                        .and_then(|frame| {
                            frame
                                .to_json_string()
                                .map_err(|error| ModError::Protocol(error.to_string()))
                        });
                    if let Ok(line) = line {
                        let mut input = input.lock().await;
                        let _ = input.write_all(line.as_bytes()).await;
                        let _ = input.write_all(b"\n").await;
                        let _ = input.flush().await;
                    }
                });
            }
        }
    }
}

struct GenerationApiSessionEntry {
    session: Arc<dyn ModSessionContext>,
    references: usize,
}

type GenerationApiSessions = Arc<StdMutex<HashMap<String, GenerationApiSessionEntry>>>;

fn retain_generation_api_session(sessions: &GenerationApiSessions, ticket: &str) -> bool {
    let mut sessions = sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = sessions.get_mut(ticket) else {
        return false;
    };
    if entry
        .session
        .generation_cancellation_token()
        .is_some_and(|token| token.is_cancelled())
    {
        return false;
    }
    entry.references = entry.references.saturating_add(1);
    true
}

fn release_generation_api_session(sessions: &GenerationApiSessions, ticket: &str) {
    let mut sessions = sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let remove = match sessions.get_mut(ticket) {
        Some(entry) if entry.references > 1 => {
            entry.references -= 1;
            false
        }
        Some(_) => true,
        None => false,
    };
    if remove {
        sessions.remove(ticket);
    }
}

struct ReleaseGenerationApiSessionOnDrop {
    sessions: GenerationApiSessions,
    ticket: String,
}

impl Drop for ReleaseGenerationApiSessionOnDrop {
    fn drop(&mut self) {
        release_generation_api_session(&self.sessions, &self.ticket);
    }
}

#[cfg(test)]
struct TestWorkerRoutePause {
    key: String,
    entered: oneshot::Sender<(String, bool)>,
    release: oneshot::Receiver<()>,
}

/// A test-only synchronization point in the actual worker stdout router. It
/// lets the generation-reset test stop one callback API message after ticket
/// retention but before the background API loop receives it.
#[cfg(test)]
#[derive(Default)]
struct TestWorkerRouteGate {
    pause: StdMutex<Option<TestWorkerRoutePause>>,
    expired_callback_error: StdMutex<Option<oneshot::Sender<()>>>,
}

#[cfg(test)]
impl TestWorkerRouteGate {
    fn pause_state_key(
        &self,
        key: &str,
    ) -> (oneshot::Receiver<(String, bool)>, oneshot::Sender<()>) {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let (release_sender, release_receiver) = oneshot::channel();
        let previous = self
            .pause
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(TestWorkerRoutePause {
                key: key.to_owned(),
                entered: entered_sender,
                release: release_receiver,
            });
        assert!(
            previous.is_none(),
            "only one worker route pause may be armed"
        );
        (entered_receiver, release_sender)
    }

    fn expect_expired_callback_error(&self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        let previous = self
            .expired_callback_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(sender);
        assert!(
            previous.is_none(),
            "only one callback error may be observed"
        );
        receiver
    }

    async fn pause_before_delivery(
        &self,
        message: &Utf16JsonProjection,
        retained_ticket: Option<&str>,
    ) {
        let pause = {
            let mut armed = self
                .pause
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let matches = armed.as_ref().is_some_and(|pause| {
                message.string_at("/kind") == Some("api")
                    && projection_u64(message, "/id") == Some(0)
                    && message.string_at("/method") == Some("state.set")
                    && projection_string(message, "/generationContextTicket")
                        .is_some_and(|ticket| !ticket.is_empty())
                    && message.string_at("/input/key") == Some(pause.key.as_str())
            });
            matches.then(|| armed.take()).flatten()
        };
        if let Some(pause) = pause {
            let ticket = projection_string(message, "/generationContextTicket")
                .expect("the gated API request carries a generation ticket");
            let retained = retained_ticket == Some(ticket.as_str());
            let _ = pause.entered.send((ticket, retained));
            let _ = pause.release.await;
        }
    }

    fn observe_expired_callback_error(&self, message: &Utf16JsonProjection) {
        let is_expired_callback_error = message.string_at("/kind") == Some("log.error")
            && message
                .string_at("/message")
                .is_some_and(|text| text.contains("originating Mod dispatch generation ended"));
        if is_expired_callback_error {
            if let Some(sender) = self
                .expired_callback_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = sender.send(());
            }
        }
    }
}

async fn route_worker_output(
    mut output: Lines<BufReader<ChildStdout>>,
    routes: Routes,
    generation_api_sessions: GenerationApiSessions,
    epoch: Weak<ModWorkerEpoch>,
    failure_sender: mpsc::UnboundedSender<ModWorkerFailure>,
    #[cfg(test)] test_route_gate: Arc<TestWorkerRouteGate>,
) {
    let mut exit_reason = "worker exited".to_owned();
    loop {
        let message = match output.next_line().await {
            Ok(Some(line)) => match parse_mod_worker_line(&line) {
                Ok(message) => message,
                Err(error) => {
                    tracing::warn!(%error, "invalid Mod worker message");
                    continue;
                }
            },
            Ok(None) => break,
            Err(error) => {
                exit_reason = error.to_string();
                tracing::warn!(%error, "Mod worker output failed");
                break;
            }
        };
        let Some(id) = projection_u64(&message, "/id") else {
            tracing::warn!("Mod worker message missing request id");
            continue;
        };
        let mut retained_ticket = None;
        match message.string_at("/kind") {
            Some("api.context.retain") | Some("api.context.release") => {
                let Some(ticket) = projection_string(&message, "/generationContextTicket") else {
                    tracing::warn!("Mod worker API context control is missing its ticket");
                    continue;
                };
                if message.string_at("/kind") == Some("api.context.retain") {
                    if !retain_generation_api_session(&generation_api_sessions, &ticket) {
                        tracing::debug!(ticket, "ignored retain for an expired Mod API session");
                    }
                } else {
                    release_generation_api_session(&generation_api_sessions, &ticket);
                }
                continue;
            }
            Some("api") if id == 0 => {
                if let Some(ticket) = projection_string(&message, "/generationContextTicket") {
                    if retain_generation_api_session(&generation_api_sessions, &ticket) {
                        retained_ticket = Some(ticket);
                    } else {
                        tracing::debug!(ticket, "queued API call for an expired Mod generation");
                    }
                }
            }
            Some("log" | "toast" | "status") if id == 0 => {
                if let Some(ticket) = projection_string(&message, "/generationContextTicket") {
                    if retain_generation_api_session(&generation_api_sessions, &ticket) {
                        retained_ticket = Some(ticket);
                    } else {
                        tracing::debug!(
                            ticket,
                            "queued raw UI output for an expired Mod generation"
                        );
                    }
                }
            }
            _ => {}
        }
        #[cfg(test)]
        {
            test_route_gate.observe_expired_callback_error(&message);
            test_route_gate
                .pause_before_delivery(&message, retained_ticket.as_deref())
                .await;
        }
        let sender = routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned();
        let delivered = sender.is_some_and(|sender| sender.send(message).is_ok());
        if !delivered {
            if let Some(ticket) = retained_ticket {
                release_generation_api_session(&generation_api_sessions, &ticket);
            }
        }
    }
    routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    if let Some(epoch) = epoch.upgrade() {
        report_worker_epoch_failure(&epoch, &failure_sender, exit_reason, None);
    }
}

fn report_worker_epoch_failure(
    epoch: &ModWorkerEpoch,
    failure_sender: &mpsc::UnboundedSender<ModWorkerFailure>,
    reason: String,
    attributed_storage_id: Option<String>,
) {
    epoch.status.store(WORKER_EPOCH_DEAD, Ordering::Release);
    if !epoch.failure_reported.swap(true, Ordering::AcqRel) {
        let _ = failure_sender.send(ModWorkerFailure {
            epoch: epoch.id,
            reason,
            attributed_storage_id,
        });
    }
}

fn message_generation_context_ticket(
    message: &Utf16JsonProjection,
) -> Result<Option<String>, ModError> {
    match message.get_property("generationContextTicket") {
        None => Ok(None),
        Some(_) => projection_string(message, "/generationContextTicket")
            .filter(|ticket| !ticket.is_empty())
            .map(Some)
            .ok_or_else(|| ModError::Protocol("invalid Mod generation context ticket".into())),
    }
}

fn background_output_context(
    worker: &ModHost,
    epoch: &Arc<ModWorkerEpoch>,
    message: &Utf16JsonProjection,
) -> Result<
    (
        Arc<dyn ModSessionContext>,
        Option<ReleaseGenerationApiSessionOnDrop>,
    ),
    ModError,
> {
    match message.get_property("generationContextTicket") {
        Some(_) => {
            let ticket =
                projection_string(message, "/generationContextTicket").ok_or_else(|| {
                    ModError::Protocol("raw Mod UI output has an invalid generation ticket".into())
                })?;
            let lease = ReleaseGenerationApiSessionOnDrop {
                sessions: epoch.generation_api_sessions.clone(),
                ticket: ticket.clone(),
            };
            let session =
                worker.generation_api_session_in(&epoch.generation_api_sessions, &ticket)?;
            Ok((session, Some(lease)))
        }
        None => worker
            .background_session()
            .map(|session| (session, None))
            .ok_or_else(|| ModError::Unavailable("Mod session ended".into())),
    }
}

async fn background_api_loop(
    host: Weak<ModHost>,
    epoch: Weak<ModWorkerEpoch>,
    mut replies: mpsc::UnboundedReceiver<Utf16JsonProjection>,
) {
    type BackgroundCall = Pin<
        Box<
            dyn Future<
                    Output = (
                        u64,
                        Option<Result<HostApiValue, ModError>>,
                        Weak<dyn ModSessionContext>,
                        Option<Arc<dyn ModSessionContext>>,
                    ),
                > + Send,
        >,
    >;
    let mut calls: FuturesUnordered<BackgroundCall> = FuturesUnordered::new();
    let mut aborts = HashMap::<u64, ApiCallControl>::new();
    let mut bound_sessions = HashMap::<u64, Weak<dyn ModSessionContext>>::new();
    let mut sweep = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            message = replies.recv() => {
                let Some(message) = message else { break };
                let Some(worker) = host.upgrade() else { break };
                let Some(source_epoch) = epoch.upgrade() else { break };
                match message.string_at("/kind") {
                    Some("api") => {
                        let Some(call_id) = projection_u64(&message, "/callId") else { continue };
                        let generation_ticket = match message_generation_context_ticket(&message) {
                            Ok(ticket) => ticket,
                            Err(error) => {
                                let _ = worker.send_generated_in_epoch(&source_epoch, &json!({"id":0,"kind":"api.error","callId":call_id,"message":error.to_string()})).await;
                                continue;
                            }
                        };
                        let generation_context_lease = if let Some(ticket) = generation_ticket.as_ref() {
                            Some(ReleaseGenerationApiSessionOnDrop {
                                sessions: source_epoch.generation_api_sessions.clone(),
                                ticket: ticket.clone(),
                            })
                        } else {
                            None
                        };
                        let api_context = match worker.resolve_api_context(&message, None) {
                            Ok(context) => context,
                            Err(error) => {
                                let _ = worker.send_generated_in_epoch(&source_epoch, &json!({"id":0,"kind":"api.error","callId":call_id,"message":error.to_string()})).await;
                                continue;
                            }
                        };
                        let method = projection_string(&message, "/method").unwrap_or_default();
                        let (bound_session, context_owner) = if let Some(ticket) = generation_ticket.as_deref() {
                            match worker.generation_api_session_in(&source_epoch.generation_api_sessions, ticket) {
                                Ok(session) => (Arc::downgrade(&session), Some(session)),
                                Err(error) => {
                                    let _ = worker.send_generated_in_epoch(&source_epoch, &json!({"id":0,"kind":"api.error","callId":call_id,"message":error.to_string()})).await;
                                    continue;
                                }
                            }
                        } else {
                            let context = worker.background_context.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                            let Some(context) = context.filter(|session| session.strong_count() > 0) else {
                                let _ = worker.send_generated_in_epoch(&source_epoch, &json!({"id":0,"kind":"api.error","callId":call_id,"message":"Mod session ended"})).await;
                                continue;
                            };
                            (context, None)
                        };
                        let (abort, registration) = AbortHandle::new_pair();
                        let cancellation = lingxi_core::host::CancellationToken::new();
                        let control = ApiCallControl::new(
                            abort,
                            cancellation.clone(),
                            method == "tool.call",
                        );
                        let generation_cancellation = context_owner.as_ref()
                            .and_then(|session| session.generation_cancellation_token());
                        let api_session = WeakModSessionContext(bound_session.clone());
                        let input_projection = message
                            .get_property("input")
                            .and_then(|_| message.subprojection("/input").ok());
                        aborts.insert(call_id, control.clone());
                        bound_sessions.insert(call_id, bound_session.clone());
                        let tool_input = input_projection.clone();
                        let origin = json!({
                            "plugin":api_context.caller.plugin,
                            "tier":projection_string(&message, "/tier").as_deref().unwrap_or("user")
                        });
                        let storage_id = projection_string(&message, "/storageId").unwrap_or_else(|| "unknown".to_owned());
                        let hook_id = projection_u64(&message, "/hookId");
                        calls.push(Box::pin(async move {
                            let _generation_context_lease = generation_context_lease;
                            let _context_owner = context_owner;
                            let context_slot = Arc::new(StdMutex::new(None));
                            let context_for_call = context_slot.clone();
                            let api_context_for_call = api_context.clone();
                            let execution = Abortable::new(async {
                                let tool_call_context = if method == "tool.call" {
                                    let Some(input) = tool_input.as_ref() else {
                                        return (Err(ModError::Protocol("tool.call is missing input".into())), None);
                                    };
                                    match acquire_tool_call_context(
                                        Some(&api_session),
                                        cancellation.clone(),
                                        input,
                                        &api_context_for_call,
                                    )
                                    .await
                                    {
                                        Ok(context) => Some(context),
                                        Err(error) => return (Err(error), None),
                                    }
                                } else {
                                    None
                                };
                                *context_for_call
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    tool_call_context.clone();
                                let _cancel_tool_call_on_drop =
                                    CancelToolCallOnDrop::new(tool_call_context.as_ref());
                                let cwd = api_session.cwd();
                                let result = worker.dispatch_host_api_with_projection(
                                    &method,
                                    input_projection,
                                    &cwd,
                                    Some(&api_session),
                                    false,
                                    0,
                                    origin,
                                    api_context_for_call,
                                    storage_id,
                                    hook_id,
                                    0,
                                    tool_call_context.clone(),
                                    generation_ticket.clone(),
                                )
                                .await;
                                (result, tool_call_context)
                            }, registration);
                            let execution = if let Some(generation_cancellation) = generation_cancellation {
                                tokio::select! {
                                    biased;
                                    _ = generation_cancellation.cancelled() => {
                                        control.cancel();
                                        None
                                    }
                                    result = execution => result.ok(),
                                }
                            } else {
                                execution.await.ok()
                            };
                            let completed = execution.is_some();
                            let result = execution.map(|(result, _context)| result);
                            let tool_context = context_slot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone();
                            if let Some(tool_context) = tool_context.as_ref() {
                                if result.as_ref().is_none_or(Result::is_err) {
                                    let _ = abort_tool_call(&api_session, tool_context).await;
                                }
                            }
                            if completed {
                                control.finish();
                            }
                            (call_id, result, bound_session, _context_owner)
                        }));
                    }
                    Some("api.cancel") => {
                        if let Some(call_id) = projection_u64(&message, "/callId") {
                            if let Some(control) = aborts.get(&call_id) { control.cancel(); }
                        }
                    }
                    Some("log") => {
                        let plugin = projection_string(&message, "/plugin").unwrap_or_else(|| "unknown".to_owned());
                        let display_text = message.string_at("/text").unwrap_or("");
                        match background_output_context(&worker, &source_epoch, &message) {
                            Ok((session, _generation_context_lease)) => {
                                if message.string_at("/to") == Some("debug") {
                                    tracing::debug!(plugin, text = display_text, "background Mod log");
                                } else {
                                    if let Ok(text) = projection_root_string_member_or_empty(&message, "/text") {
                                        session.emit_mod_log(&plugin, text).await;
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::debug!(%error, plugin, "discarded raw background Mod log");
                            }
                        }
                    }
                    Some("toast") => {
                        match background_output_context(&worker, &source_epoch, &message) {
                            Ok((session, _generation_context_lease)) => {
                                let plugin = projection_string(&message, "/plugin").unwrap_or_else(|| "unknown".to_owned());
                                let timeout_ms = projection_u64(&message, "/timeoutMs").unwrap_or(4000);
                                if let Ok(text) = projection_root_string_member_or_empty(&message, "/text") {
                                    session.emit_mod_toast(&plugin, text, timeout_ms).await;
                                }
                            }
                            Err(error) => {
                                tracing::debug!(%error, "discarded raw background Mod toast");
                            }
                        }
                    }
                    Some("notify") => {
                        match background_output_context(&worker, &source_epoch, &message) {
                            Ok((session, _generation_context_lease)) => {
                                let plugin = projection_string(&message, "/plugin")
                                    .unwrap_or_else(|| "engine".to_owned());
                                if let Ok(text) = projection_root_string_member_or_empty(&message, "/text") {
                                    session.emit_mod_notify(&plugin, text).await;
                                }
                            }
                            Err(error) => {
                                tracing::debug!(%error, "discarded raw background Mod notify");
                            }
                        }
                    }
                    Some("status") => {
                        match background_output_context(&worker, &source_epoch, &message) {
                            Ok((session, _generation_context_lease)) => {
                                let plugin = projection_string(&message, "/plugin").unwrap_or_else(|| "unknown".to_owned());
                                if let Ok(text) = projection_optional_string_member(&message, "/text") {
                                    session.emit_mod_status(&plugin, text).await;
                                }
                            }
                            Err(error) => {
                                tracing::debug!(%error, "discarded raw background Mod status");
                            }
                        }
                    }
                    Some("ui.client.event") => {
                        if let Some(session) = worker.background_session() {
                            if let Err(error) = worker
                                .handle_client_ui_background_event(&message, session.as_ref())
                                .await
                            {
                                tracing::warn!(%error, "background Mod Client UI event failed");
                            }
                        }
                    }
                    Some("log.error") => tracing::warn!(message = %message.to_json_string().unwrap_or_default(), "background Mod API call failed"),
                    Some("protocol.error") => {
                        if let Some(call_id) = projection_u64(&message, "/callId") {
                            let _ = worker.send_generated_in_epoch(&source_epoch, &json!({
                                "id":0,
                                "kind":"api.error",
                                "callId":call_id,
                                "message":message.string_at("/message")
                                    .unwrap_or("invalid Mod worker protocol reply"),
                            })).await;
                        } else {
                            tracing::warn!(message = %message.to_json_string().unwrap_or_default(), "unmatched background Mod protocol error");
                        }
                    }
                    Some("progress") => {},
                    _ => tracing::warn!(message = %message.to_json_string().unwrap_or_default(), "unexpected background Mod reply"),
                }
            }
            completed = calls.next(), if !calls.is_empty() => {
                if let Some((call_id, result, bound_session, _context_owner)) = completed {
                    let cancelled_without_result =
                        tool_call_cancelled_without_result(aborts.get(&call_id), &result);
                    aborts.remove(&call_id);
                    bound_sessions.remove(&call_id);
                    if let Some(worker) = host.upgrade() {
                        let Some(source_epoch) = epoch.upgrade() else { break };
                        if bound_session.upgrade().is_none_or(|session| {
                            session.generation_cancellation_token().is_some_and(|token| token.is_cancelled())
                        }) {
                            let _ = worker.send_generated_in_epoch(&source_epoch, &json!({"id":0,"kind":"api.error","callId":call_id,"message":"Mod session ended"})).await;
                            continue;
                        }
                        if cancelled_without_result {
                            let _ = worker.send_generated_in_epoch(&source_epoch, &json!({
                                "id":0,
                                "kind":"api.error",
                                "callId":call_id,
                                "message":TOOL_CALL_ABORTED_MESSAGE
                            })).await;
                            continue;
                        }
                        match result {
                            Some(Ok(value)) => {
                                if let Ok(reply) = value.reply(0, call_id) {
                                    let _ = worker.send_projection_in_epoch(&source_epoch, &reply).await;
                                }
                            }
                            Some(Err(error)) => { let _ = worker.send_generated_in_epoch(&source_epoch, &json!({"id":0,"kind":"api.error","callId":call_id,"message":error.to_string()})).await; }
                            None => {},
                        }
                    }
                }
            }
            _ = sweep.tick(), if !calls.is_empty() => {
                if let Some(worker) = host.upgrade() {
                    let Some(source_epoch) = epoch.upgrade() else { break };
                    let ended = bound_sessions.iter().filter_map(|(call_id, session)| {
                        session.upgrade().is_none_or(|session| {
                            session.generation_cancellation_token().is_some_and(|token| token.is_cancelled())
                        })
                            .then_some(*call_id)
                    }).collect::<Vec<_>>();
                    for call_id in ended {
                        bound_sessions.remove(&call_id);
                        if let Some(control) = aborts.remove(&call_id) {
                            control.cancel();
                            let _ = worker.send_generated_in_epoch(&source_epoch, &json!({"id":0,"kind":"api.error","callId":call_id,"message":"Mod session ended"})).await;
                        }
                    }
                }
            }
        }
    }
}

/// A parsed, linked Mod module held in the worker until policy accepts it.
/// The later load evaluates these exact source bytes after `plugin.register`.
pub struct ModPreparedModule {
    token: String,
    pub uses: Value,
    client_modules: Option<PreparedClientModules>,
}

/// The first `next(e)` has reached the core, or every hook answered without
/// passing the spawn on. Claude's Agent tool must inspect a forwarded request
/// and start the child before the pending `next(e)` can resolve.
pub enum ModAgentSpawnAdmission {
    Forwarded {
        input: Utf16JsonProjection,
        start: ModAgentSpawnStart,
    },
    Answered(Utf16JsonProjection),
}

/// Completes the suspended `agent.spawn` core after the real child starts.
/// Dropping this without a receipt rejects the pending core and lets the Mod
/// chain run its `.catch` / failure path rather than reporting a phantom child.
pub struct ModAgentSpawnStart {
    completion: Option<watch::Sender<Option<Result<Utf16JsonProjection, String>>>>,
    dispatch: Option<tokio::task::JoinHandle<Result<Utf16JsonProjection, ModError>>>,
}

struct AbortModDispatchOnDrop(Option<tokio::task::AbortHandle>);

impl Drop for AbortModDispatchOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

impl ModAgentSpawnStart {
    pub fn started(mut self, agent_id: String, model: String) {
        if let Some(completion) = self.completion.take() {
            let result = projection_object(vec![
                ("agentId".into(), root_string_projection(&agent_id)),
                ("model".into(), root_string_projection(&model)),
            ])
            .expect("host-generated Agent spawn result is valid JSON");
            completion.send_replace(Some(Ok(result)));
        }
        // Hooks may run after `await next(e)`. The Mod dispatch remains owned
        // by its session worker and finishes asynchronously, as upstream does.
        self.dispatch.take();
    }

    pub fn failed(mut self, reason: String) {
        if let Some(completion) = self.completion.take() {
            completion.send_replace(Some(Err(reason)));
        }
        self.dispatch.take();
    }
}

impl Drop for ModAgentSpawnStart {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            completion.send_replace(Some(Err(
                "agent.spawn: the subagent did not start (the Agent tool refused the spawn after the hooks passed it on)".into(),
            )));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModUiPressSite {
    session_id: String,
    surface: String,
    component: String,
    request_id: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModUiPressTarget {
    site: ModUiPressSite,
    plugin: Vec<u16>,
    element: Vec<u16>,
    handle: u64,
    worker_epoch: String,
    render_revision: u64,
    site_revision: u64,
}

#[derive(Default)]
struct ModUiPressRegistry {
    revisions: HashMap<ModUiPressSite, u64>,
    targets: HashSet<ModUiPressTarget>,
}

/// A session-owned Mod worker. Request ids route nested `next(e)` dispatches
/// without keeping a worker-wide lock while Rust core actions are running.
pub struct ModHost {
    instance_id: u64,
    registration_revision: AtomicU64,
    worker_epoch: RwLock<Arc<ModWorkerEpoch>>,
    next_worker_epoch: AtomicU64,
    active_request_epochs: Arc<StdMutex<HashMap<u64, Weak<ModWorkerEpoch>>>>,
    worker_failure_sender: mpsc::UnboundedSender<ModWorkerFailure>,
    worker_failure_receiver: StdMutex<Option<mpsc::UnboundedReceiver<ModWorkerFailure>>>,
    bun_executable: PathBuf,
    worker_bootstrap: Arc<ModWorkerBootstrap>,
    next_id: AtomicU64,
    store_root: Option<PathBuf>,
    background_context: Arc<StdMutex<Option<Weak<dyn ModSessionContext>>>>,
    api_origin_contexts: ModApiOriginContexts,
    agent_spawn_budget: agent_api::ModAgentSpawnBudget,
    model_policy: StdMutex<Option<Arc<dyn ModSettingsReader>>>,
    env_scans: StdMutex<HashMap<String, scan::EnvScan>>,
    loaded_events: StdMutex<HashMap<String, Vec<String>>>,
    environment: StdMutex<HashMap<String, Option<String>>>,
    state: Arc<StdMutex<ModStateStore>>,
    sec_default_order: AtomicI64,
    ui_render_generation: Arc<AtomicU64>,
    next_state_ui_invalidation_id: Arc<AtomicU64>,
    ui_pacing_clock_origin: std::time::Instant,
    ui_pacing_cancelled: Arc<std::sync::atomic::AtomicBool>,
    ui_press_registry: StdMutex<ModUiPressRegistry>,
    client_ui: client_ui_host::ClientUiHostState,
    #[cfg(test)]
    test_route_gate: Arc<TestWorkerRouteGate>,
}

impl Drop for ModHost {
    fn drop(&mut self) {
        self.ui_pacing_cancelled.store(true, Ordering::Release);
        let epoch = Arc::clone(self.worker_epoch.get_mut());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = epoch.child.lock().await.kill().await;
            });
        }
    }
}

/// One lazily opened bottom stream for a `turn.step` chain. The finish action
/// reads the original model response summary after the last chunk was pulled.
pub struct ModStreamSource {
    stream: BoxStream<'static, Result<Utf16JsonProjection, ModError>>,
    finish: Box<dyn FnOnce() -> Result<Utf16JsonProjection, ModError> + Send>,
}

impl ModStreamSource {
    pub fn new<S, F>(stream: S, finish: F) -> Self
    where
        S: futures_util::Stream<Item = Result<Utf16JsonProjection, ModError>> + Send + 'static,
        F: FnOnce() -> Result<Utf16JsonProjection, ModError> + Send + 'static,
    {
        Self {
            stream: stream.boxed(),
            finish: Box::new(finish),
        }
    }
}

enum StreamTask {
    Opened {
        call_id: u64,
        source: Result<ModStreamSource, ModError>,
    },
    Pulled {
        call_id: u64,
        source_id: u64,
        source: ModStreamSource,
        item: Option<Result<Utf16JsonProjection, ModError>>,
    },
    Api {
        call_id: u64,
        result: Option<Result<HostApiValue, ModError>>,
    },
}

#[derive(Clone)]
struct ModStateValue {
    value: lingxi_core::types::utf16_json::Utf16JsonProjection,
    version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModUiRenderSiteKey {
    surface: String,
    component: String,
    request_id: Vec<u16>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum ModUiRenderPace {
    #[default]
    Steady,
    Live,
}

impl ModUiRenderPace {
    const fn interval_ms(self) -> u64 {
        match self {
            Self::Steady => 100,
            Self::Live => 34,
        }
    }

    fn for_site(surface: &str, component: &str, on_screen: bool) -> Self {
        if surface == "terminal"
            && (component == "AbovePrompt"
                || component == "PromptHint"
                || (component == "Pane" && on_screen))
        {
            Self::Live
        } else {
            Self::Steady
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ModUiPacingTimerKind {
    Plugin {
        plugin: String,
        pace: ModUiRenderPace,
    },
    StateFold(ModUiRenderPace),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModUiPacingTimer {
    kind: ModUiPacingTimerKind,
    generation: u64,
    deadline_ms: u64,
}

#[derive(Default)]
struct ModUiInvalidationUpdate {
    sites: Vec<ModUiRenderSiteKey>,
    timers: Vec<ModUiPacingTimer>,
    version_changed: bool,
}

impl ModUiInvalidationUpdate {
    fn append(&mut self, mut other: Self) {
        self.sites.append(&mut other.sites);
        self.timers.append(&mut other.timers);
        self.version_changed |= other.version_changed;
    }
}

#[derive(Default)]
struct ModUiPacedCounter {
    last_at_ms: Option<u64>,
    pending: Option<(u64, u64)>,
    generation: u64,
}

impl ModUiPacedCounter {
    fn call(
        &mut self,
        plugin: &str,
        pace: ModUiRenderPace,
        now_ms: u64,
    ) -> (bool, Option<ModUiPacingTimer>) {
        if self.pending.is_some() {
            return (false, None);
        }
        let interval_ms = pace.interval_ms();
        let elapsed_ms = self
            .last_at_ms
            .map_or(interval_ms, |last_at_ms| now_ms.saturating_sub(last_at_ms));
        let wait_ms = interval_ms.saturating_sub(elapsed_ms);
        if wait_ms == 0 {
            self.bump(now_ms);
            return (true, None);
        }
        self.generation = self.generation.saturating_add(1);
        let deadline_ms = now_ms.saturating_add(wait_ms);
        let generation = self.generation;
        self.pending = Some((deadline_ms, generation));
        (
            false,
            Some(ModUiPacingTimer {
                kind: ModUiPacingTimerKind::Plugin {
                    plugin: plugin.to_owned(),
                    pace,
                },
                generation,
                deadline_ms,
            }),
        )
    }

    fn bump(&mut self, now_ms: u64) {
        self.last_at_ms = Some(now_ms);
        self.pending = None;
        self.generation = self.generation.saturating_add(1);
    }

    fn take_due(&mut self, generation: u64, now_ms: u64) -> bool {
        let Some((deadline_ms, pending_generation)) = self.pending else {
            return false;
        };
        if pending_generation != generation || deadline_ms > now_ms {
            return false;
        }
        self.bump(now_ms);
        true
    }
}

#[derive(Default)]
struct ModUiStateFold {
    last_at_ms: Option<u64>,
    pending: Option<(u64, u64)>,
    generation: u64,
    stale_sites: HashSet<ModUiRenderSiteKey>,
}

impl ModUiStateFold {
    fn schedule(
        &mut self,
        site: ModUiRenderSiteKey,
        pace: ModUiRenderPace,
        now_ms: u64,
    ) -> (Option<ModUiPacingTimer>, Option<Vec<ModUiRenderSiteKey>>) {
        self.stale_sites.insert(site);
        if self.pending.is_some() {
            return (None, None);
        }
        let interval_ms = pace.interval_ms();
        let elapsed_ms = self
            .last_at_ms
            .map_or(interval_ms, |last_at_ms| now_ms.saturating_sub(last_at_ms));
        let wait_ms = interval_ms.saturating_sub(elapsed_ms);
        if wait_ms == 0 {
            return (None, Some(self.flush(now_ms)));
        }
        self.generation = self.generation.saturating_add(1);
        let deadline_ms = now_ms.saturating_add(wait_ms);
        let generation = self.generation;
        self.pending = Some((deadline_ms, generation));
        (
            Some(ModUiPacingTimer {
                kind: ModUiPacingTimerKind::StateFold(pace),
                generation,
                deadline_ms,
            }),
            None,
        )
    }

    fn flush(&mut self, now_ms: u64) -> Vec<ModUiRenderSiteKey> {
        self.pending = None;
        self.last_at_ms = Some(now_ms);
        self.generation = self.generation.saturating_add(1);
        std::mem::take(&mut self.stale_sites).into_iter().collect()
    }

    fn take_due(&mut self, generation: u64, now_ms: u64) -> Option<Vec<ModUiRenderSiteKey>> {
        let Some((deadline_ms, pending_generation)) = self.pending else {
            return None;
        };
        if pending_generation != generation || deadline_ms > now_ms {
            return None;
        }
        Some(self.flush(now_ms))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModUiRenderStateReadScope {
    site: ModUiRenderSiteKey,
    revision: u64,
    on_screen: bool,
}

async fn emit_ui_render_site_invalidations_for_session(
    host_instance_id: u64,
    next_invalidation_id: &AtomicU64,
    sites: Vec<ModUiRenderSiteKey>,
    session: Option<&dyn ModSessionContext>,
) {
    let Some(session) = session else {
        return;
    };
    if sites.is_empty() {
        return;
    }
    let instances = sites
        .iter()
        .map(|site| {
            projection_object(vec![
                ("surface".into(), root_string_projection(&site.surface)),
                ("component".into(), root_string_projection(&site.component)),
                (
                    "instance_id".into(),
                    projection_from_string_units(&site.request_id)?,
                ),
            ])
        })
        .collect::<Result<Vec<_>, ModError>>()
        .and_then(|instances| {
            Utf16JsonProjection::array(instances)
                .map_err(|error| ModError::Protocol(error.to_string()))
        });
    let instances = match instances {
        Ok(instances) => instances,
        Err(error) => {
            tracing::warn!(%error, "cannot build exact Mod UI invalidation instances");
            return;
        }
    };
    let sequence = next_invalidation_id.fetch_add(1, Ordering::Relaxed) + 1;
    let uuid = format!("mod-ui-state-{host_instance_id}-{sequence}");
    let session_id = session.id().await;
    session
        .emit_mod_ui_invalidate(Some(&instances), &uuid, &session_id)
        .await;
}

#[derive(Default)]
struct ModUiRenderSiteState {
    token: u64,
    global_render_generation: u64,
    pace: ModUiRenderPace,
    has_clients: bool,
    render_plugins: Vec<String>,
    plugin_versions: HashMap<String, u64>,
    reads: HashSet<ModStateKey>,
}

#[derive(Default)]
struct ModStateStore {
    values: HashMap<ModStateKey, ModStateValue>,
    version_floor: u64,
    ui_render_sites: HashMap<ModUiRenderSiteKey, ModUiRenderSiteState>,
    pending_ui_render_reads: HashMap<(ModUiRenderSiteKey, u64), HashMap<ModStateKey, u64>>,
    ui_render_plugin_versions: HashMap<(String, ModUiRenderPace), u64>,
    ui_render_plugin_pacing: HashMap<String, (ModUiPacedCounter, ModUiPacedCounter)>,
    ui_render_slow_plugins: HashMap<String, u64>,
    ui_render_state_folds: (ModUiStateFold, ModUiStateFold),
    client_ui_global_render_generation: u64,
}

impl ModStateStore {
    fn note_ui_render_read(
        &mut self,
        scope: &ModUiRenderStateReadScope,
        key: ModStateKey,
        version: u64,
    ) {
        self.pending_ui_render_reads
            .entry((scope.site.clone(), scope.revision))
            .or_default()
            .entry(key)
            .or_insert(version);
    }

    fn plugin_is_slow(&self, plugin: &str, now_ms: u64) -> bool {
        self.ui_render_slow_plugins
            .get(plugin)
            .is_some_and(|until_ms| now_ms < *until_ms)
    }

    fn mark_ui_render_plugins_slow<'a>(
        &mut self,
        plugins: impl IntoIterator<Item = &'a str>,
        now_ms: u64,
    ) {
        for plugin in plugins {
            self.ui_render_slow_plugins
                .insert(plugin.to_owned(), now_ms.saturating_add(1_000));
        }
    }

    fn bump_plugin_render_counter(
        &mut self,
        plugin: &str,
        pace: ModUiRenderPace,
    ) -> Vec<ModUiRenderSiteKey> {
        let version = {
            let version = self
                .ui_render_plugin_versions
                .entry((plugin.to_owned(), pace))
                .or_default();
            *version = version.saturating_add(1);
            *version
        };
        let mut affected = Vec::new();
        for (site_key, site) in &mut self.ui_render_sites {
            if site.pace == pace && site.render_plugins.iter().any(|name| name == plugin) {
                site.token = site.token.saturating_add(1);
                site.plugin_versions.insert(plugin.to_owned(), version);
                affected.push(site_key.clone());
            }
        }
        affected
    }

    fn request_ui_render_plugin_invalidation(
        &mut self,
        plugin: &str,
        active_live_site: bool,
        now_ms: u64,
    ) -> ModUiInvalidationUpdate {
        let slow = self.plugin_is_slow(plugin, now_ms);
        let (steady_bumped, steady_timer, live_bumped, live_timer) = {
            let (steady, live) = self
                .ui_render_plugin_pacing
                .entry(plugin.to_owned())
                .or_default();
            let (steady_bumped, steady_timer) =
                steady.call(plugin, ModUiRenderPace::Steady, now_ms);
            if steady_bumped {
                // Native's steady scheduler uses the live scheduler as its
                // onBump callback, so one reached steady bump updates both
                // captured pace counters at the same clock instant.
                live.bump(now_ms);
                (steady_bumped, steady_timer, false, None)
            } else if !slow && active_live_site {
                let (live_bumped, live_timer) = live.call(plugin, ModUiRenderPace::Live, now_ms);
                (steady_bumped, steady_timer, live_bumped, live_timer)
            } else {
                (steady_bumped, steady_timer, false, None)
            }
        };
        let mut update = ModUiInvalidationUpdate::default();
        if let Some(timer) = steady_timer {
            update.timers.push(timer);
        }
        if let Some(timer) = live_timer {
            update.timers.push(timer);
        }
        if steady_bumped {
            update
                .sites
                .extend(self.bump_plugin_render_counter(plugin, ModUiRenderPace::Steady));
            update
                .sites
                .extend(self.bump_plugin_render_counter(plugin, ModUiRenderPace::Live));
            update.version_changed = true;
        } else if live_bumped {
            update
                .sites
                .extend(self.bump_plugin_render_counter(plugin, ModUiRenderPace::Live));
            update.version_changed = true;
        }
        update
    }

    fn fire_ui_render_pacing_timer(
        &mut self,
        timer: &ModUiPacingTimer,
        now_ms: u64,
    ) -> ModUiInvalidationUpdate {
        match &timer.kind {
            ModUiPacingTimerKind::Plugin { plugin, pace } => {
                let due =
                    self.ui_render_plugin_pacing
                        .get_mut(plugin)
                        .is_some_and(|(steady, live)| match pace {
                            ModUiRenderPace::Steady => steady.take_due(timer.generation, now_ms),
                            ModUiRenderPace::Live => live.take_due(timer.generation, now_ms),
                        });
                if !due {
                    return ModUiInvalidationUpdate::default();
                }
                let mut update = ModUiInvalidationUpdate::default();
                update
                    .sites
                    .extend(self.bump_plugin_render_counter(plugin, *pace));
                if *pace == ModUiRenderPace::Steady {
                    if let Some((_, live)) = self.ui_render_plugin_pacing.get_mut(plugin) {
                        live.bump(now_ms);
                    }
                    update
                        .sites
                        .extend(self.bump_plugin_render_counter(plugin, ModUiRenderPace::Live));
                }
                update.version_changed = true;
                update
            }
            ModUiPacingTimerKind::StateFold(pace) => {
                let sites = match pace {
                    ModUiRenderPace::Steady => self
                        .ui_render_state_folds
                        .0
                        .take_due(timer.generation, now_ms),
                    ModUiRenderPace::Live => self
                        .ui_render_state_folds
                        .1
                        .take_due(timer.generation, now_ms),
                };
                let Some(sites) = sites else {
                    return ModUiInvalidationUpdate::default();
                };
                self.bump_state_site_tokens(&sites);
                ModUiInvalidationUpdate {
                    version_changed: !sites.is_empty(),
                    sites,
                    timers: Vec::new(),
                }
            }
        }
    }

    fn schedule_state_reader_invalidation(
        &mut self,
        site: &ModUiRenderSiteKey,
        now_ms: u64,
    ) -> ModUiInvalidationUpdate {
        let Some(site_state) = self.ui_render_sites.get(site) else {
            return ModUiInvalidationUpdate::default();
        };
        let pace = if site_state
            .render_plugins
            .iter()
            .any(|plugin| self.plugin_is_slow(plugin, now_ms))
        {
            ModUiRenderPace::Steady
        } else {
            site_state.pace
        };
        let scheduled = match pace {
            ModUiRenderPace::Steady => {
                self.ui_render_state_folds
                    .0
                    .schedule(site.clone(), pace, now_ms)
            }
            ModUiRenderPace::Live => {
                self.ui_render_state_folds
                    .1
                    .schedule(site.clone(), pace, now_ms)
            }
        };
        let (timer, immediate) = scheduled;
        let mut update = ModUiInvalidationUpdate::default();
        if let Some(timer) = timer {
            update.timers.push(timer);
        }
        if let Some(sites) = immediate {
            self.bump_state_site_tokens(&sites);
            update.version_changed = !sites.is_empty();
            update.sites = sites;
        }
        update
    }

    fn schedule_ui_render_write(
        &mut self,
        key: &ModStateKey,
        now_ms: u64,
    ) -> ModUiInvalidationUpdate {
        let affected = self
            .ui_render_sites
            .iter()
            .filter(|(_, site)| site.reads.contains(key))
            .map(|(site, _)| site.clone())
            .collect::<Vec<_>>();
        let mut update = ModUiInvalidationUpdate::default();
        for site in affected {
            update.append(self.schedule_state_reader_invalidation(&site, now_ms));
        }
        update
    }

    fn flush_ui_render_state_folds(&mut self, now_ms: u64) -> ModUiInvalidationUpdate {
        let steady = self.ui_render_state_folds.0.flush(now_ms);
        let live = self.ui_render_state_folds.1.flush(now_ms);
        let mut update = ModUiInvalidationUpdate::default();
        if !steady.is_empty() {
            self.bump_state_site_tokens(&steady);
            update.sites.extend(steady);
            update.version_changed = true;
        }
        if !live.is_empty() {
            self.bump_state_site_tokens(&live);
            update.sites.extend(live);
            update.version_changed = true;
        }
        update
    }

    fn remove_ui_render_site(&mut self, site: &ModUiRenderSiteKey) {
        self.ui_render_sites.remove(site);
        self.ui_render_state_folds.0.stale_sites.remove(site);
        self.ui_render_state_folds.1.stale_sites.remove(site);
        self.pending_ui_render_reads
            .retain(|(pending_site, _), _| pending_site != site);
    }

    fn bump_state_site_tokens(&mut self, sites: &[ModUiRenderSiteKey]) {
        for site_key in sites {
            if let Some(site) = self.ui_render_sites.get_mut(site_key) {
                site.token = site.token.saturating_add(1);
            }
        }
    }

    fn active_live_site_for_plugin(&self, plugin: &str) -> bool {
        self.ui_render_sites.values().any(|site| {
            site.pace == ModUiRenderPace::Live
                && site.render_plugins.iter().any(|name| name == plugin)
        })
    }

    fn note_client_ui_global_render_change(&mut self) -> Vec<ModUiRenderSiteKey> {
        self.client_ui_global_render_generation =
            self.client_ui_global_render_generation.saturating_add(1);
        self.ui_render_sites.keys().cloned().collect()
    }

    fn current_client_ui_render_token(&mut self, site: &ModUiRenderSiteKey) -> u64 {
        let generation = self.client_ui_global_render_generation;
        let Some(site_state) = self.ui_render_sites.get_mut(site) else {
            return 0;
        };
        if site_state.global_render_generation != generation {
            site_state.token = site_state.token.saturating_add(1);
            site_state.global_render_generation = generation;
        }
        site_state.token
    }

    fn finish_ui_render(
        &mut self,
        site: ModUiRenderSiteKey,
        revision: u64,
        render_plugins: Vec<String>,
        has_clients: bool,
        pace: ModUiRenderPace,
        now_ms: u64,
    ) -> (Option<String>, ModUiInvalidationUpdate) {
        let reads = self
            .pending_ui_render_reads
            .remove(&(site.clone(), revision))
            .unwrap_or_default();
        self.pending_ui_render_reads
            .retain(|(pending_site, _), _| pending_site != &site);

        let mut plugin_versions = HashMap::new();
        for plugin in &render_plugins {
            plugin_versions.insert(
                plugin.clone(),
                self.ui_render_plugin_versions
                    .get(&(plugin.clone(), pace))
                    .copied()
                    .unwrap_or_default(),
            );
        }

        let read_versions_changed = reads.iter().any(|(key, version)| {
            self.values.get(key).map_or(0, |entry| entry.version) != *version
        });
        let global_render_generation = self.client_ui_global_render_generation;
        let token_site = site.clone();
        {
            let site_state = self.ui_render_sites.entry(site).or_default();
            let render_version_changed = site_state.render_plugins != render_plugins
                || site_state.plugin_versions != plugin_versions
                || site_state.pace != pace
                || site_state.has_clients != has_clients;
            let global_render_changed =
                site_state.global_render_generation != global_render_generation;
            if render_version_changed || global_render_changed {
                site_state.token = site_state.token.saturating_add(1);
            }
            site_state.global_render_generation = global_render_generation;
            site_state.pace = pace;
            site_state.has_clients = has_clients;
            site_state.render_plugins = render_plugins;
            site_state.plugin_versions = plugin_versions;
            site_state.reads = reads.into_keys().collect();
        }
        let mut update = ModUiInvalidationUpdate::default();
        if read_versions_changed {
            update = self.schedule_state_reader_invalidation(&token_site, now_ms);
        }
        let token = self
            .ui_render_sites
            .get(&token_site)
            .and_then(|site| site.has_clients.then(|| site.token.to_string()));
        (token, update)
    }

    fn discard_ui_render(&mut self, site: &ModUiRenderSiteKey, revision: u64) {
        self.pending_ui_render_reads
            .remove(&(site.clone(), revision));
    }
}

async fn spawn_mod_worker(
    bun_executable: &Path,
    worker_entry: &Path,
) -> Result<(Child, ChildStdin, ChildStdout), ModError> {
    let mut child = Command::new(bun_executable)
        .arg("--no-warnings")
        .arg(worker_entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ModError::Unavailable(error.to_string()))?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| ModError::Unavailable("stdin unavailable".into()))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| ModError::Unavailable("stdout unavailable".into()))?;
    Ok((child, input, output))
}

impl ModHost {
    /// Run a Mod `turn.step` stream with one-chunk-at-a-time backpressure. The
    /// source callback is invoked only when the bottom of the hook chain is
    /// pulled, so a hook's rewritten model/effort reaches the actual request.
    pub async fn dispatch_turn_step_stream<F, Fut, C, CFut>(
        &self,
        input: Utf16JsonProjection,
        source: F,
        on_chunk: C,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Utf16JsonProjection) -> Fut,
        Fut: Future<Output = Result<ModStreamSource, ModError>> + Send,
        C: FnMut(Utf16JsonProjection) -> CFut,
        CFut: Future<Output = Result<(), ModError>>,
    {
        let session = self.background_session();
        let cwd = session
            .as_ref()
            .map_or_else(std::env::current_dir, |session| Ok(session.cwd()))?;
        self.dispatch_turn_step_stream_at(input, &cwd, source, on_chunk)
            .await
    }

    /// Dispatch a child stream with the child's effective working directory.
    pub async fn dispatch_turn_step_stream_at<F, Fut, C, CFut>(
        &self,
        input: Utf16JsonProjection,
        agent_cwd: &Path,
        source: F,
        on_chunk: C,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Utf16JsonProjection) -> Fut,
        Fut: Future<Output = Result<ModStreamSource, ModError>> + Send,
        C: FnMut(Utf16JsonProjection) -> CFut,
        CFut: Future<Output = Result<(), ModError>>,
    {
        self.dispatch_turn_step_stream_at_and_origin(
            input,
            agent_cwd,
            lingxi_core::host::task_registry::FieldPresence::Missing,
            source,
            on_chunk,
        )
        .await
    }

    /// Dispatch a child stream while retaining its host-owned origin in each
    /// API closure created by the worker.
    pub async fn dispatch_turn_step_stream_at_and_origin<F, Fut, C, CFut>(
        &self,
        input: Utf16JsonProjection,
        agent_cwd: &Path,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        mut source: F,
        mut on_chunk: C,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Utf16JsonProjection) -> Fut,
        Fut: Future<Output = Result<ModStreamSource, ModError>> + Send,
        C: FnMut(Utf16JsonProjection) -> CFut,
        CFut: Future<Output = Result<(), ModError>>,
    {
        let session = self.background_session();
        let generation_session = session
            .as_ref()
            .and_then(|session| session.ui_invalidation_context());
        let cwd = agent_cwd.to_path_buf();
        let visible_origin = match &api_origin {
            lingxi_core::host::task_registry::FieldPresence::Value(value) => Some(value.clone()),
            lingxi_core::host::task_registry::FieldPresence::Missing
            | lingxi_core::host::task_registry::FieldPresence::Null => None,
        };
        let mut dispatch_message = generated_projection(&json!({
            "kind":"stream.dispatch",
            "event":"turn.step",
            "cwd":cwd,
            "origin":visible_origin,
        }))?;
        set_projection_field(&mut dispatch_message, "input", input)?;
        let mut request = self
            .request_projection_with_api_origin_and_session(
                dispatch_message,
                api_origin,
                generation_session,
                None,
            )
            .await?;
        let mut sources = HashMap::<u64, ModStreamSource>::new();
        type Pending<'a> = Pin<Box<dyn Future<Output = StreamTask> + Send + 'a>>;
        let mut tasks: FuturesUnordered<Pending<'_>> = FuturesUnordered::new();
        let mut api_abort = HashMap::<u64, ApiCallControl>::new();
        loop {
            let message = if tasks.is_empty() {
                self.receive(&mut request).await?
            } else {
                tokio::select! {
                    completed = tasks.next() => {
                        if let Some(task) = completed {
                            match task {
                                StreamTask::Opened { call_id, source } => match source {
                                    Ok(source) => {
                                        sources.insert(call_id, source);
                                        self.send_generated(&json!({"id":request.id,"kind":"source.open.result","callId":call_id,"sourceId":call_id})).await?;
                                    }
                                    Err(error) => self.send_generated(&json!({"id":request.id,"kind":"source.open.error","callId":call_id,"message":error.to_string()})).await?,
                                },
                                StreamTask::Pulled { call_id, source_id, source, item } => match item {
                                    Some(Ok(chunk)) => {
                                        sources.insert(source_id, source);
                                        let mut reply = generated_projection(&json!({"id":request.id,"kind":"source.pull.result","callId":call_id,"done":false}))?;
                                        set_projection_field(&mut reply, "chunk", chunk)?;
                                        self.send_projection(&reply).await?;
                                    }
                                    Some(Err(error)) => self.send_generated(&json!({"id":request.id,"kind":"source.pull.error","callId":call_id,"message":error.to_string()})).await?,
                                    None => match (source.finish)() {
                                        Ok(result) => {
                                            let mut reply = generated_projection(&json!({"id":request.id,"kind":"source.pull.result","callId":call_id,"done":true}))?;
                                            set_projection_field(&mut reply, "result", result)?;
                                            self.send_projection(&reply).await?;
                                        }
                                        Err(error) => self.send_generated(&json!({"id":request.id,"kind":"source.pull.error","callId":call_id,"message":error.to_string()})).await?,
                                    }
                                },
                                StreamTask::Api { call_id, result } => {
                                    let cancelled_without_result =
                                        tool_call_cancelled_without_result(
                                            api_abort.get(&call_id),
                                            &result,
                                        );
                                    api_abort.remove(&call_id);
                                    if cancelled_without_result {
                                        self.send_generated(&json!({
                                            "id":request.id,
                                            "kind":"api.error",
                                            "callId":call_id,
                                            "message":TOOL_CALL_ABORTED_MESSAGE
                                        })).await?;
                                    } else {
                                        match result {
                                            Some(Ok(value)) => {
                                                let reply = value.reply(request.id, call_id)?;
                                                self.send_projection(&reply).await?;
                                            }
                                            Some(Err(error)) => self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?,
                                            None => {},
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    reply = request.replies.recv() => reply.ok_or_else(|| ModError::Unavailable("worker exited".into()))?,
                }
            };
            let kind = message.string_at("/kind");
            match kind {
                Some("stream.ready") => {
                    self.send_generated(&json!({"id":request.id,"kind":"stream.advance"}))
                        .await?;
                }
                Some("source.open") => {
                    let call_id = projection_u64(&message, "/callId")
                        .ok_or_else(|| ModError::Protocol("source.open missing callId".into()))?;
                    let forwarded = message
                        .subprojection("/input")
                        .map_err(|_| ModError::Protocol("source.open missing input".into()))?;
                    let opening = source(forwarded);
                    tasks.push(Box::pin(async move {
                        StreamTask::Opened {
                            call_id,
                            source: opening.await,
                        }
                    }));
                }
                Some("source.pull") => {
                    let call_id = projection_u64(&message, "/callId")
                        .ok_or_else(|| ModError::Protocol("source.pull missing callId".into()))?;
                    let source_id = projection_u64(&message, "/sourceId")
                        .ok_or_else(|| ModError::Protocol("source.pull missing sourceId".into()))?;
                    let Some(mut source) = sources.remove(&source_id) else {
                        self.send_generated(&json!({"id":request.id,"kind":"source.pull.error","callId":call_id,"message":"source is not open or is already being pulled"})).await?;
                        continue;
                    };
                    tasks.push(Box::pin(async move {
                        let item = source.stream.next().await;
                        StreamTask::Pulled {
                            call_id,
                            source_id,
                            source,
                            item,
                        }
                    }));
                }
                Some("api") => {
                    let call_id = projection_u64(&message, "/callId")
                        .ok_or_else(|| ModError::Protocol("api request missing callId".into()))?;
                    let api_context = match self.resolve_api_context(&message, Some(&request)) {
                        Ok(context) => context,
                        Err(error) => {
                            self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?;
                            continue;
                        }
                    };
                    let method = message
                        .string_at("/method")
                        .ok_or_else(|| ModError::Protocol("api request missing method".into()))?
                        .to_owned();
                    let origin = json!({
                        "plugin":api_context.caller.plugin,
                        "tier":message.string_at("/tier").unwrap_or("user")
                    });
                    let (abort, registration) = AbortHandle::new_pair();
                    let cancellation = lingxi_core::host::CancellationToken::new();
                    let control =
                        ApiCallControl::new(abort, cancellation.clone(), method == "tool.call");
                    api_abort.insert(call_id, control.clone());
                    let request_id = request.id;
                    let api_session = session.clone();
                    let api_cwd = cwd.clone();
                    let input_projection = match message.at_pointer("/input") {
                        Some(_) => Some(
                            message
                                .subprojection("/input")
                                .map_err(|error| ModError::Protocol(error.to_string()))?,
                        ),
                        None => None,
                    };
                    if method == "tool.call" && input_projection.is_none() {
                        self.send_generated(&json!({
                            "id":request.id,
                            "kind":"api.error",
                            "callId":call_id,
                            "message":"tool.call requires input"
                        }))
                        .await?;
                        continue;
                    }
                    let input_for_tool = input_projection.clone();
                    let storage_id = message
                        .string_at("/storageId")
                        .unwrap_or("unknown")
                        .to_owned();
                    let hook_id = projection_u64(&message, "/hookId");
                    let generation_context_ticket = message
                        .string_at("/generationContextTicket")
                        .map(str::to_owned);
                    tasks.push(Box::pin(async move {
                        let context_slot = Arc::new(StdMutex::new(None));
                        let context_for_call = context_slot.clone();
                        let api_context_for_call = api_context.clone();
                        let execution = Abortable::new(
                            async {
                                let tool_call_context = if method == "tool.call" {
                                    let input = input_for_tool
                                        .as_ref()
                                        .expect("tool.call input validated above");
                                    match acquire_tool_call_context(
                                        api_session.as_deref(),
                                        cancellation.clone(),
                                        input,
                                        &api_context_for_call,
                                    )
                                    .await
                                    {
                                        Ok(context) => Some(context),
                                        Err(error) => return (Err(error), None),
                                    }
                                } else {
                                    None
                                };
                                *context_for_call
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    tool_call_context.clone();
                                let _cancel_tool_call_on_drop =
                                    CancelToolCallOnDrop::new(tool_call_context.as_ref());
                                let result = self
                                    .dispatch_host_api_with_projection(
                                        &method,
                                        input_projection,
                                        &api_cwd,
                                        api_session.as_deref(),
                                        false,
                                        0,
                                        origin,
                                        api_context_for_call,
                                        storage_id,
                                        hook_id,
                                        request_id,
                                        tool_call_context.clone(),
                                        generation_context_ticket,
                                    )
                                    .await;
                                (result, tool_call_context)
                            },
                            registration,
                        )
                        .await
                        .ok();
                        let completed = execution.is_some();
                        let result = execution.map(|(result, _context)| result);
                        let tool_call_context = context_slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clone();
                        if let (Some(context), Some(session)) =
                            (tool_call_context.as_ref(), api_session.as_deref())
                        {
                            if result.as_ref().is_none_or(Result::is_err) {
                                let _ = abort_tool_call(session, context).await;
                            }
                        }
                        if completed {
                            control.finish();
                        }
                        StreamTask::Api { call_id, result }
                    }));
                }
                Some("api.cancel") => {
                    if let Some(call_id) = projection_u64(&message, "/callId") {
                        if let Some(control) = api_abort.get(&call_id) {
                            control.cancel();
                        }
                    }
                }
                Some("stream.chunk") => {
                    let chunk = message
                        .subprojection("/chunk")
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                    on_chunk(chunk).await?;
                    self.send_generated(&json!({"id":request.id,"kind":"stream.advance"}))
                        .await?;
                }
                Some("stream.result") => {
                    request.completed = true;
                    return message
                        .subprojection("/result")
                        .map_err(|error| ModError::Protocol(error.to_string()));
                }
                Some("log") => {
                    let plugin = message.string_at("/plugin").unwrap_or("unknown");
                    let text = lossy_string(&message, "/text").unwrap_or_default();
                    if message.string_at("/to") == Some("debug") {
                        tracing::debug!(plugin, text, "mod log");
                    } else if let Some(session) = session.as_ref() {
                        session
                            .emit_mod_log(
                                plugin,
                                message
                                    .subprojection("/text")
                                    .unwrap_or_else(|_| root_string_projection("")),
                            )
                            .await;
                    }
                }
                Some("toast") => {
                    if let Some(session) = session.as_ref() {
                        session
                            .emit_mod_toast(
                                message.string_at("/plugin").unwrap_or("unknown"),
                                message
                                    .subprojection("/text")
                                    .unwrap_or_else(|_| root_string_projection("")),
                                projection_u64(&message, "/timeoutMs").unwrap_or(4000),
                            )
                            .await;
                    }
                }
                Some("notify") => {
                    if let (Some(session), Ok(text)) =
                        (session.as_ref(), message.subprojection("/text"))
                    {
                        session
                            .emit_mod_notify(message.string_at("/plugin").unwrap_or("engine"), text)
                            .await;
                    }
                }
                Some("status") => {
                    if let Some(session) = session.as_ref() {
                        session
                            .emit_mod_status(
                                message.string_at("/plugin").unwrap_or("unknown"),
                                message
                                    .at_pointer("/text")
                                    .and_then(|_| message.subprojection("/text").ok()),
                            )
                            .await;
                    }
                }
                Some("log.error") | Some("progress") => {}
                Some("stream.error") | Some("error") => {
                    return Err(ModError::Hook(
                        message
                            .string_at("/message")
                            .unwrap_or("unknown stream error")
                            .to_owned(),
                    ));
                }
                _ => {
                    return Err(ModError::Protocol(format!(
                        "unexpected stream reply: {}",
                        message.to_json_string().unwrap_or_default()
                    )));
                }
            }
        }
    }

    /// Start a two-phase `agent.spawn` dispatch. `next(e)` exposes the first
    /// forwarded request immediately, then waits for `ModAgentSpawnStart` to
    /// report the real child id/model. A hook that answers without `next(e)`
    /// returns `Answered` and must not cause any child to start.
    pub async fn begin_agent_spawn(
        self: &Arc<Self>,
        input: Utf16JsonProjection,
        provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
    ) -> Result<ModAgentSpawnAdmission, ModError> {
        let visible_origin = match &provenance.hook_origin {
            lingxi_core::host::task_registry::FieldPresence::Value(value) => Some(value.clone()),
            lingxi_core::host::task_registry::FieldPresence::Missing
            | lingxi_core::host::task_registry::FieldPresence::Null => None,
        };
        let session = self.background_session();
        let cwd = session
            .as_ref()
            .map_or_else(std::env::current_dir, |session| Ok(session.cwd()))?;
        let log_session = session.clone();
        let toast_session = session.clone();
        let status_session = session.clone();
        let (arrived_tx, mut arrived_rx) = oneshot::channel::<Utf16JsonProjection>();
        let arrived_tx = Arc::new(StdMutex::new(Some(arrived_tx)));
        let (completion_tx, completion_rx) =
            watch::channel::<Option<Result<Utf16JsonProjection, String>>>(None);
        let host = Arc::clone(self);
        let mut dispatch = tokio::spawn(async move {
            host.dispatch_with_ui_at_context_and_api_origin(
                "agent.spawn",
                input,
                &cwd,
                session.as_deref(),
                None,
                visible_origin,
                None,
                None,
                None,
                None,
                provenance.hook_origin.clone(),
                move |forwarded| {
                    let sender = arrived_tx.lock().unwrap().take();
                    let mut completion = completion_rx.clone();
                    async move {
                        let forwarded = forwarded.ok_or_else(|| {
                            ModError::Hook("agent.spawn next input is required".into())
                        })?;
                        if let Some(sender) = sender {
                            let _ = sender.send(forwarded.clone());
                        }
                        loop {
                            if let Some(result) = completion.borrow().clone() {
                                return result.map(Some).map_err(ModError::Hook);
                            }
                            completion.changed().await.map_err(|_| {
                                ModError::Unavailable(
                                    "agent.spawn startup receipt was dropped".into(),
                                )
                            })?;
                        }
                    }
                },
                move |plugin, text| {
                    let session = log_session.clone();
                    async move {
                        if let Some(session) = session {
                            session.emit_mod_log(&plugin, text).await;
                        }
                    }
                },
                move |plugin, text, timeout_ms| {
                    let session = toast_session.clone();
                    async move {
                        if let Some(session) = session {
                            session.emit_mod_toast(&plugin, text, timeout_ms).await;
                        }
                    }
                },
                move |plugin, text| {
                    let session = status_session.clone();
                    async move {
                        if let Some(session) = session {
                            session.emit_mod_status(&plugin, text).await;
                        }
                    }
                },
            )
            .await
            .map(|outcome| outcome.result_projection)
        });
        let mut abort = AbortModDispatchOnDrop(Some(dispatch.abort_handle()));
        let admission = tokio::select! {
            biased;
            arrived = &mut arrived_rx => match arrived {
                Ok(input) => Ok(ModAgentSpawnAdmission::Forwarded {
                    input,
                    start: ModAgentSpawnStart {
                        completion: Some(completion_tx),
                        dispatch: Some(dispatch),
                    },
                }),
                Err(_) => dispatch.await.map_err(|error| ModError::Unavailable(error.to_string()))?
                    .map(ModAgentSpawnAdmission::Answered),
            },
            result = &mut dispatch => result
                .map_err(|error| ModError::Unavailable(error.to_string()))?
                .map(ModAgentSpawnAdmission::Answered),
        };
        abort.0 = None;
        admission
    }

    /// Parse and scan a candidate module without evaluating its top-level code.
    pub async fn prepare_module(
        &self,
        root: &Path,
        module: &Path,
    ) -> Result<ModPreparedModule, ModError> {
        let prepared_sources =
            prepare_plugin_sources(root, module, None, &client_surface_runtime_sources())
                .await
                .map_err(|error| {
                    ModError::Unavailable(format!("Plugin source preparation failed: {error}"))
                })?;
        let source_graph = prepared_sources
            .hook_worker_graph()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let mut request = self
            .request_projection(generated_projection(
                &json!({"kind":"prepare","root":root,"module":module,"sourceGraph":source_graph}),
            )?)
            .await?;
        let reply = self.receive(&mut request).await?;
        match reply.string_at("/kind") {
            Some("prepared") => {
                let token = projection_string(&reply, "/token")
                    .ok_or_else(|| ModError::Protocol("prepared module has no token".into()))?;
                let source_count = reply.array_len("/sources").ok_or_else(|| {
                    ModError::Protocol("prepared module has no source scan".into())
                })?;
                let sources = (0..source_count)
                    .map(|index| {
                        projection_string(&reply, &format!("/sources/{index}")).ok_or_else(|| {
                            ModError::Protocol("prepared module source scan is malformed".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let uses = tokio::task::spawn_blocking(move || scan::scan_uses(&sources))
                    .await
                    .map_err(|error| ModError::Unavailable(error.to_string()))?
                    .map_err(ModError::Hook)?;
                Ok(ModPreparedModule {
                    token,
                    uses,
                    client_modules: prepared_sources.clients,
                })
            }
            Some("error") => Err(ModError::Hook(
                reply
                    .string_at("/message")
                    .unwrap_or("unknown error")
                    .into(),
            )),
            _ => Err(ModError::Protocol(format!(
                "unexpected prepare reply: {}",
                reply.to_json_string().unwrap_or_default()
            ))),
        }
    }

    /// Release a prepared module when `plugin.register` refuses the plugin.
    pub async fn discard_prepared(&self, prepared: &ModPreparedModule) -> Result<(), ModError> {
        let mut request = self
            .request_projection(generated_projection(
                &json!({"kind":"discard","token":prepared.token}),
            )?)
            .await?;
        let reply = self.receive(&mut request).await?;
        match reply.string_at("/kind") {
            Some("discarded") => Ok(()),
            Some("error") => Err(ModError::Hook(
                reply
                    .string_at("/message")
                    .unwrap_or("unknown error")
                    .into(),
            )),
            _ => Err(ModError::Protocol(format!(
                "unexpected discard reply: {}",
                reply.to_json_string().unwrap_or_default()
            ))),
        }
    }

    /// Ask already-loaded Mods whether a candidate may enter the session.
    /// This runs before evaluating the candidate or registering any of its
    /// declarative components.
    pub async fn dispatch_plugin_register(
        &self,
        input: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ModError> {
        let session = self.background_session();
        let cwd = input
            .string_at("/root")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                session
                    .as_ref()
                    .map_or_else(PathBuf::new, |session| session.cwd())
            });
        let log_session = session.clone();
        let toast_session = session.clone();
        let status_session = session.clone();
        let outcome = self
            .dispatch_with_ui_at_context(
                "plugin.register",
                input,
                &cwd,
                session.as_deref(),
                None,
                None,
                None,
                None,
                |_| async { Ok(Some(static_projection(r#"{"allow":true}"#))) },
                move |plugin, text| {
                    let session = log_session.clone();
                    async move {
                        if let Some(session) = session {
                            session.emit_mod_log(&plugin, text).await;
                        }
                    }
                },
                move |plugin, text, timeout_ms| {
                    let session = toast_session.clone();
                    async move {
                        if let Some(session) = session {
                            session.emit_mod_toast(&plugin, text, timeout_ms).await;
                        }
                    }
                },
                move |plugin, text| {
                    let session = status_session.clone();
                    async move {
                        if let Some(session) = session {
                            session.emit_mod_status(&plugin, text).await;
                        }
                    }
                },
            )
            .await?;
        let result = outcome.result_projection;
        if result.at_pointer("/allow").and_then(JsonValueView::as_bool) == Some(true)
            || result.string_units("/refuse").is_some()
        {
            Ok(result)
        } else {
            Err(ModError::Hook(
                "plugin.register returned neither allow nor refuse".into(),
            ))
        }
    }

    /// Launch a killable Bun worker. The executable may be supplied by a
    /// packaged host; `bun` from PATH is the developer/CLI default.
    pub async fn start(bun_executable: Option<&Path>) -> Result<Arc<Self>, ModError> {
        Self::start_with_store_root(bun_executable, None).await
    }

    /// Launch a Bun worker whose `$.store` is kept under the session's config home.
    pub async fn start_with_store_root(
        bun_executable: Option<&Path>,
        store_root: Option<PathBuf>,
    ) -> Result<Arc<Self>, ModError> {
        let from_host = std::env::var_os(MOD_BUN_EXECUTABLE_ENV);
        let executable = bun_executable
            .map(Path::to_path_buf)
            .or_else(|| from_host.as_deref().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("bun"));
        let worker_bootstrap = Arc::new(ModWorkerBootstrap::create()?);
        let (child, input, output) = spawn_mod_worker(&executable, &worker_bootstrap.entry).await?;
        let (epoch, background_replies) =
            ModWorkerEpoch::new(1, child, input, Some(worker_bootstrap.clone()), false);
        let (worker_failure_sender, worker_failure_receiver) = mpsc::unbounded_channel();
        #[cfg(test)]
        let test_route_gate = Arc::new(TestWorkerRouteGate::default());
        let host = Arc::new(Self {
            instance_id: NEXT_MOD_HOST_ID.fetch_add(1, Ordering::Relaxed),
            registration_revision: AtomicU64::new(0),
            worker_epoch: RwLock::new(epoch.clone()),
            next_worker_epoch: AtomicU64::new(2),
            active_request_epochs: Arc::new(StdMutex::new(HashMap::new())),
            worker_failure_sender,
            worker_failure_receiver: StdMutex::new(Some(worker_failure_receiver)),
            bun_executable: executable,
            worker_bootstrap,
            next_id: AtomicU64::new(1),
            store_root,
            background_context: Arc::new(StdMutex::new(None)),
            api_origin_contexts: new_api_origin_contexts(),
            agent_spawn_budget: agent_api::ModAgentSpawnBudget::default(),
            model_policy: StdMutex::new(None),
            env_scans: StdMutex::new(HashMap::new()),
            loaded_events: StdMutex::new(HashMap::new()),
            environment: StdMutex::new(HashMap::new()),
            state: Arc::new(StdMutex::new(ModStateStore::default())),
            sec_default_order: AtomicI64::new(i64::MIN),
            ui_render_generation: Arc::new(AtomicU64::new(0)),
            next_state_ui_invalidation_id: Arc::new(AtomicU64::new(0)),
            ui_pacing_clock_origin: std::time::Instant::now(),
            ui_pacing_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ui_press_registry: StdMutex::new(ModUiPressRegistry::default()),
            client_ui: client_ui_host::ClientUiHostState::default(),
            #[cfg(test)]
            test_route_gate,
        });
        host.start_epoch_tasks(&epoch, output, background_replies)
            .await;
        Ok(host)
    }

    async fn start_epoch_tasks(
        self: &Arc<Self>,
        epoch: &Arc<ModWorkerEpoch>,
        output: ChildStdout,
        background_replies: mpsc::UnboundedReceiver<Utf16JsonProjection>,
    ) {
        let router_task = tokio::spawn(route_worker_output(
            BufReader::new(output).lines(),
            epoch.routes.clone(),
            epoch.generation_api_sessions.clone(),
            Arc::downgrade(epoch),
            self.worker_failure_sender.clone(),
            #[cfg(test)]
            self.test_route_gate.clone(),
        ));
        let background_api_task = tokio::spawn(background_api_loop(
            Arc::downgrade(self),
            Arc::downgrade(epoch),
            background_replies,
        ));
        *epoch.router_task.lock().await = Some(router_task);
        *epoch.background_api_task.lock().await = Some(background_api_task);
    }

    /// Take the single failure stream consumed by the owning PluginManager.
    pub fn subscribe_worker_failures(&self) -> Option<mpsc::UnboundedReceiver<ModWorkerFailure>> {
        self.worker_failure_receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    #[cfg(test)]
    async fn terminate_current_worker_for_test(&self) {
        let epoch = self.current_worker_epoch().await;
        let _ = epoch.child.lock().await.kill().await;
    }

    /// Replace only the child epoch, then replay the current Manager-declared
    /// set. Requests on the dead epoch are never retried; their channels retain
    /// their original routes and stdin until their callers return.
    pub async fn recover_epoch(
        self: &Arc<Self>,
        modules: &[ModReplayModule],
    ) -> Result<Vec<ModReplayFailure>, ModError> {
        MOD_WORKER_RECOVERY_SCOPE
            .scope(true, self.recover_epoch_inner(modules))
            .await
    }

    async fn recover_epoch_inner(
        self: &Arc<Self>,
        modules: &[ModReplayModule],
    ) -> Result<Vec<ModReplayFailure>, ModError> {
        let old_epoch = self.current_worker_epoch().await;
        if old_epoch
            .status
            .compare_exchange(
                WORKER_EPOCH_DEAD,
                WORKER_EPOCH_RECOVERING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(ModError::Unavailable(
                "worker epoch is not awaiting recovery".into(),
            ));
        }

        {
            let mut child = old_epoch.child.lock().await;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if let Some(task) = old_epoch.router_task.lock().await.take() {
            let mut task = task;
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        if let Some(task) = old_epoch.background_api_task.lock().await.take() {
            let mut task = task;
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }

        let mut storage_ids = self
            .loaded_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        storage_ids.extend(modules.iter().map(|module| module.storage_id.clone()));
        let session = self.background_session();
        for storage_id in &storage_ids {
            self.api_origin_contexts.unregister_storage(storage_id);
            self.client_ui.unload_storage(storage_id);
            if let Some(session) = session.as_ref() {
                session.tool_unregister_plugin(storage_id);
                session.command_unregister_plugin(storage_id).await;
            }
        }
        self.env_scans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.loaded_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        old_epoch
            .generation_api_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.registration_revision.fetch_add(1, Ordering::AcqRel);
        self.clear_all_mod_ui_press_targets();
        self.note_client_ui_global_render_change().await;

        let epoch_id = self.next_worker_epoch.fetch_add(1, Ordering::Relaxed);
        let (child, input, output) =
            match spawn_mod_worker(&self.bun_executable, &self.worker_bootstrap.entry).await {
                Ok(worker) => worker,
                Err(error) => {
                    old_epoch.status.store(WORKER_EPOCH_DEAD, Ordering::Release);
                    return Err(error);
                }
            };
        let (new_epoch, background_replies) = ModWorkerEpoch::new(
            epoch_id,
            child,
            input,
            Some(self.worker_bootstrap.clone()),
            true,
        );
        *self.worker_epoch.write().await = new_epoch.clone();
        self.start_epoch_tasks(&new_epoch, output, background_replies)
            .await;

        let mut failures = Vec::new();
        for module in modules {
            if new_epoch.status.load(Ordering::Acquire) == WORKER_EPOCH_DEAD {
                return Err(ModError::Unavailable(
                    "replacement worker exited during module replay".into(),
                ));
            }
            let prepared = match self.prepare_module(&module.root, &module.module).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    failures.push(ModReplayFailure {
                        storage_id: module.storage_id.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            let mut registration = serde_json::Map::new();
            registration.insert("name".into(), Value::String(module.plugin.clone()));
            registration.insert("tier".into(), Value::String(module.tier.clone()));
            registration.insert(
                "root".into(),
                Value::String(module.root.to_string_lossy().into_owned()),
            );
            if let Some(version) = &module.version {
                registration.insert("version".into(), Value::String(version.clone()));
            }
            registration.insert(
                "provenance".into(),
                Value::String(module.provenance.clone()),
            );
            registration.insert("uses".into(), prepared.uses.clone());
            let decision = self
                .dispatch_plugin_register(generated_projection(&Value::Object(registration))?)
                .await;
            let refusal = match decision {
                Ok(value) => value.string_at("/refuse").map(str::to_owned),
                Err(error) => Some(error.to_string()),
            };
            if let Some(reason) = refusal {
                let _ = self.discard_prepared(&prepared).await;
                failures.push(ModReplayFailure {
                    storage_id: module.storage_id.clone(),
                    reason,
                });
                continue;
            }
            if let Err(error) = self
                .load_with_tier_order_storage_prepared(
                    &module.plugin,
                    &module.storage_id,
                    &module.root,
                    &module.module,
                    module.options.clone(),
                    &module.tier,
                    module.tier_order,
                    Some(&prepared),
                )
                .await
            {
                failures.push(ModReplayFailure {
                    storage_id: module.storage_id.clone(),
                    reason: error.to_string(),
                });
            }
        }
        if new_epoch.status.load(Ordering::Acquire) == WORKER_EPOCH_DEAD {
            return Err(ModError::Unavailable(
                "replacement worker exited during module replay".into(),
            ));
        }
        new_epoch
            .status
            .store(WORKER_EPOCH_RUNNING, Ordering::Release);
        Ok(failures)
    }

    pub fn registration_identity(&self) -> (u64, u64) {
        (
            self.instance_id,
            self.registration_revision.load(Ordering::Acquire),
        )
    }

    fn bump_client_ui_global_render_generation(&self) -> Vec<ModUiRenderSiteKey> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .note_client_ui_global_render_change()
    }

    async fn emit_client_ui_global_render_invalidations(&self, sites: Vec<ModUiRenderSiteKey>) {
        if sites.is_empty() {
            return;
        }
        let session = self.background_session();
        self.emit_ui_render_site_invalidations(sites, session.as_deref())
            .await;
    }

    /// Advance the global Client render-version input and invalidate every
    /// committed Client site. This is the Host counterpart to Native's global
    /// `ui.render` version changes outside `$.ui.invalidate` (for example a
    /// successful CwdChanged rehome).
    pub async fn note_client_ui_global_render_change(&self) {
        let sites = self.bump_client_ui_global_render_generation();
        self.emit_client_ui_global_render_invalidations(sites).await;
    }

    /// Session-scoped generation for the live TUI render site. A Mod
    /// `$.ui.invalidate('ui.render')` advances it so the app can discard its
    /// rendered tree and request a fresh one on the next draw.
    pub fn ui_render_generation(&self) -> u64 {
        self.ui_render_generation.load(Ordering::Acquire)
    }

    pub(super) fn current_client_ui_render_state_version(
        &self,
        surface: &str,
        component: &str,
        request_id: &[u16],
    ) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current_client_ui_render_token(&ModUiRenderSiteKey {
                surface: surface.to_owned(),
                component: component.to_owned(),
                request_id: request_id.to_vec(),
            })
    }

    fn finish_client_ui_render_state(
        &self,
        surface: &str,
        component: &str,
        request_id: &[u16],
        revision: u64,
        render_plugins: Vec<String>,
        has_clients: bool,
        pace: ModUiRenderPace,
    ) -> (Option<String>, ModUiInvalidationUpdate) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish_ui_render(
                ModUiRenderSiteKey {
                    surface: surface.to_owned(),
                    component: component.to_owned(),
                    request_id: request_id.to_vec(),
                },
                revision,
                render_plugins,
                has_clients,
                pace,
                self.ui_pacing_now_ms(),
            )
    }

    fn ui_pacing_now_ms(&self) -> u64 {
        self.ui_pacing_clock_origin
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    pub(super) fn note_ui_render_slow_plugins(&self, plugins: &[String]) {
        let now_ms = self.ui_pacing_now_ms();
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mark_ui_render_plugins_slow(plugins.iter().map(String::as_str), now_ms);
    }

    async fn apply_ui_invalidation_update(
        &self,
        update: ModUiInvalidationUpdate,
        session: Option<&dyn ModSessionContext>,
    ) {
        self.emit_ui_render_site_invalidations(update.sites, session)
            .await;
        let delayed_session = session.and_then(|session| session.ui_invalidation_context());
        for timer in update.timers {
            self.schedule_ui_pacing_timer(timer, delayed_session.clone());
        }
    }

    fn schedule_ui_pacing_timer(
        &self,
        timer: ModUiPacingTimer,
        delayed_session: Option<Arc<dyn ModSessionContext>>,
    ) {
        let state = self.state.clone();
        let ui_render_generation = self.ui_render_generation.clone();
        let next_state_ui_invalidation_id = self.next_state_ui_invalidation_id.clone();
        let background_context = self
            .background_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let pacing_clock_origin = self.ui_pacing_clock_origin;
        let pacing_cancelled = self.ui_pacing_cancelled.clone();
        let instance_id = self.instance_id;
        let delay_ms = timer.deadline_ms.saturating_sub(self.ui_pacing_now_ms());

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            if pacing_cancelled.load(Ordering::Acquire) {
                return;
            }
            let now_ms = pacing_clock_origin
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let is_plugin_timer = matches!(&timer.kind, ModUiPacingTimerKind::Plugin { .. });
            let update = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fire_ui_render_pacing_timer(&timer, now_ms);
            if is_plugin_timer && update.version_changed {
                ui_render_generation.fetch_add(1, Ordering::AcqRel);
            }
            if let Some(session) =
                delayed_session.or_else(|| background_context.as_ref().and_then(Weak::upgrade))
            {
                emit_ui_render_site_invalidations_for_session(
                    instance_id,
                    &next_state_ui_invalidation_id,
                    update.sites,
                    Some(session.as_ref()),
                )
                .await;
            }
        });
    }

    pub(super) fn discard_client_ui_render_state(
        &self,
        surface: &str,
        component: &str,
        request_id: &[u16],
        revision: u64,
    ) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .discard_ui_render(
                &ModUiRenderSiteKey {
                    surface: surface.to_owned(),
                    component: component.to_owned(),
                    request_id: request_id.to_vec(),
                },
                revision,
            );
    }

    pub(super) fn remove_client_ui_render_state(
        &self,
        surface: &str,
        component: &str,
        request_id: &[u16],
    ) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove_ui_render_site(&ModUiRenderSiteKey {
                surface: surface.to_owned(),
                component: component.to_owned(),
                request_id: request_id.to_vec(),
            });
    }

    async fn emit_ui_render_site_invalidations(
        &self,
        sites: Vec<ModUiRenderSiteKey>,
        session: Option<&dyn ModSessionContext>,
    ) {
        emit_ui_render_site_invalidations_for_session(
            self.instance_id,
            &self.next_state_ui_invalidation_id,
            sites,
            session,
        )
        .await;
    }

    /// Advance the host-side revision for one session/site render. Targets are
    /// revoked at request start so an old on-screen control cannot win a race
    /// against an asynchronous redraw.
    pub fn begin_mod_ui_render(
        &self,
        session_id: &str,
        input: &Utf16JsonProjection,
        revision: u64,
    ) -> Result<u64, ModError> {
        ui::validate_above_prompt_event(input).map_err(ModError::Hook)?;
        if session_id.is_empty() || revision == 0 {
            return Err(ModError::Hook(
                "ui.render needs a session and positive site revision".into(),
            ));
        }
        let site = ModUiPressSite {
            session_id: session_id.to_owned(),
            surface: input.string_at("/surface").unwrap_or_default().to_owned(),
            component: input.string_at("/component").unwrap_or_default().to_owned(),
            request_id: input.string_units("/requestId").unwrap_or_default(),
        };
        let mut registry = self
            .ui_press_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = registry.revisions.get(&site).copied().unwrap_or(0);
        if revision >= current {
            registry.revisions.insert(site.clone(), revision);
            registry.targets.retain(|target| target.site != site);
        }
        Ok(self.registration_revision.load(Ordering::Acquire))
    }

    /// Commit only the exact render request still current for this session and
    /// site. A render from before clear/reload cannot restore old callbacks.
    pub fn finish_mod_ui_render(
        &self,
        session_id: &str,
        input: &Utf16JsonProjection,
        site_revision: u64,
        registration_revision: u64,
        tree: &Utf16JsonProjection,
    ) -> Result<(), ModError> {
        if registration_revision != self.registration_revision.load(Ordering::Acquire) {
            return Ok(());
        }
        let site = ModUiPressSite {
            session_id: session_id.to_owned(),
            surface: input.string_at("/surface").unwrap_or_default().to_owned(),
            component: input.string_at("/component").unwrap_or_default().to_owned(),
            request_id: input.string_units("/requestId").unwrap_or_default(),
        };
        let buttons = if tree.string_at("/type") == Some("engine") {
            Vec::new()
        } else {
            ui::above_prompt_lines(tree).map_err(ModError::Hook)?;
            ui::above_prompt_buttons(tree).map_err(ModError::Hook)?
        };
        let mut seen_keys = HashSet::new();
        if buttons
            .iter()
            .any(|button| !seen_keys.insert((button.plugin.clone(), button.element.clone())))
        {
            return Err(ModError::Hook(
                "AbovePrompt cannot draw two Buttons from the same plugin with the same key".into(),
            ));
        }
        let mut registry = self
            .ui_press_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.revisions.get(&site).copied() != Some(site_revision)
            || registration_revision != self.registration_revision.load(Ordering::Acquire)
        {
            return Ok(());
        }
        registry.targets.retain(|target| target.site != site);
        registry
            .targets
            .extend(buttons.into_iter().map(|button| ModUiPressTarget {
                site: site.clone(),
                plugin: button.plugin,
                element: button.element,
                handle: button.handle,
                worker_epoch: button.worker_epoch,
                render_revision: button.render_revision,
                site_revision,
            }));
        Ok(())
    }

    /// Revoke one terminal render site's actions without waiting for an old
    /// render future. A later render must carry a strictly newer revision.
    pub fn clear_mod_ui_press_actions(
        &self,
        session_id: &str,
        revision: u64,
    ) -> Result<(), ModError> {
        if session_id.is_empty() || revision == 0 {
            return Err(ModError::Hook(
                "ui action clear needs a session and positive revision".into(),
            ));
        }
        let site = ModUiPressSite {
            session_id: session_id.to_owned(),
            surface: "terminal".into(),
            component: "AbovePrompt".into(),
            request_id: "above-prompt".encode_utf16().collect(),
        };
        let mut registry = self
            .ui_press_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = registry.revisions.get(&site).copied().unwrap_or(0);
        if revision >= current {
            registry.revisions.insert(site.clone(), revision);
            registry.targets.retain(|target| target.site != site);
        }
        Ok(())
    }

    fn clear_all_mod_ui_press_targets(&self) {
        self.ui_press_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .targets
            .clear();
    }

    /// Whether any loaded module can observe this site. Used to leave the
    /// ordinary model path untouched when no stream hook is installed.
    pub fn has_event(&self, event: &str) -> bool {
        self.loaded_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|events| {
                events.iter().any(|pattern| {
                    pattern == event
                        || pattern == "*"
                        || pattern
                            .strip_suffix(".*")
                            .is_some_and(|prefix| event.starts_with(&format!("{prefix}.")))
                })
            })
    }

    /// End the current Mod session's volatile state. Keep the highest version
    /// as a floor so a write in the next session cannot reuse an old version.
    /// Calling this again after the same transition is harmless.
    pub fn reset_session_state(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.version_floor = state
            .values
            .values()
            .fold(state.version_floor, |floor, entry| floor.max(entry.version));
        state.values.clear();
    }

    /// Seat the security-default prompt and settings guards for managed policy sessions.
    /// The worker keeps it outermost even if managed plugins load later.
    pub fn set_sec_default_order(&self, order: Option<i64>) {
        self.sec_default_order
            .store(order.unwrap_or(i64::MIN), Ordering::Release);
    }

    /// Bind background timer callbacks without extending the session lifetime.
    pub fn attach_background_context(&self, session: Weak<dyn ModSessionContext>) {
        *self
            .background_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session);
    }

    fn background_session(&self) -> Option<Arc<dyn ModSessionContext>> {
        self.background_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
    }

    async fn current_worker_epoch(&self) -> Arc<ModWorkerEpoch> {
        Arc::clone(&*self.worker_epoch.read().await)
    }

    fn generation_api_session_in(
        &self,
        sessions: &GenerationApiSessions,
        ticket: &str,
    ) -> Result<Arc<dyn ModSessionContext>, ModError> {
        sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(ticket)
            .filter(|entry| {
                !entry
                    .session
                    .generation_cancellation_token()
                    .is_some_and(|token| token.is_cancelled())
            })
            .map(|entry| Arc::clone(&entry.session))
            .ok_or_else(|| {
                ModError::Unavailable("originating Mod dispatch generation ended".into())
            })
    }

    /// Upgrade the owning session for child-agent Mod events. The stored weak
    /// reference cannot extend the session lifetime by itself.
    pub fn bound_session(&self) -> Option<Arc<dyn ModSessionContext>> {
        self.background_session()
    }

    /// Bind the same managed model policy used by the owning main loop.
    pub fn attach_model_policy(&self, reader: Arc<dyn ModSettingsReader>) {
        *self
            .model_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reader);
    }

    pub async fn model_allowed(&self, model: &str) -> Result<Option<bool>, ModError> {
        let reader = self
            .model_policy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match reader {
            Some(reader) => reader.model_allowed(model).await,
            None => Ok(None),
        }
    }

    /// Serialize one flat protocol envelope directly. Dynamic JSON members
    /// never pass through serde_json::Value on this worker transport.
    async fn send_projection(&self, message: &Utf16JsonProjection) -> Result<(), ModError> {
        let request_id = projection_u64(message, "/id").ok_or_else(|| {
            ModError::Protocol("worker response is missing a safe request id".into())
        })?;
        let line = encode_mod_ipc_frame(message)?
            .to_json_string()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        self.send_serialized(request_id, line.as_bytes()).await
    }

    /// Fixed host-authored control envelopes contain no arbitrary payload.
    /// Keep their typed construction separate from the UJP wire path.
    async fn send_generated(&self, message: &Value) -> Result<(), ModError> {
        let request_id = message.get("id").and_then(Value::as_u64).ok_or_else(|| {
            ModError::Protocol("worker response is missing its request id".into())
        })?;
        let projection = generated_projection(message)?;
        let line = encode_mod_ipc_frame(&projection)?
            .to_json_string()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        self.send_serialized(request_id, line.as_bytes()).await
    }

    /// Send one already serialized JSON-line member through its owning worker
    /// epoch. Raw projected JSON uses this path so it cannot bypass lifecycle
    /// status checks or accidentally land on a replacement worker.
    async fn send_serialized(&self, request_id: u64, line: &[u8]) -> Result<(), ModError> {
        let epoch = if request_id == 0 {
            self.current_worker_epoch().await
        } else {
            self.active_request_epochs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&request_id)
                .and_then(Weak::upgrade)
                .ok_or_else(|| ModError::Unavailable("Mod request epoch has ended".into()))?
        };
        self.send_serialized_in_epoch(&epoch, request_id, line)
            .await
    }

    async fn send_projection_in_epoch(
        &self,
        epoch: &Arc<ModWorkerEpoch>,
        message: &Utf16JsonProjection,
    ) -> Result<(), ModError> {
        let request_id = projection_u64(message, "/id").ok_or_else(|| {
            ModError::Protocol("worker response is missing a safe request id".into())
        })?;
        if request_id != 0 {
            return Err(ModError::Protocol(
                "epoch-pinned background reply must use request id zero".into(),
            ));
        }
        let line = encode_mod_ipc_frame(message)?
            .to_json_string()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        self.send_serialized_in_epoch(epoch, request_id, line.as_bytes())
            .await
    }

    async fn send_generated_in_epoch(
        &self,
        epoch: &Arc<ModWorkerEpoch>,
        message: &Value,
    ) -> Result<(), ModError> {
        let request_id = message.get("id").and_then(Value::as_u64).ok_or_else(|| {
            ModError::Protocol("worker response is missing its request id".into())
        })?;
        if request_id != 0 {
            return Err(ModError::Protocol(
                "epoch-pinned background reply must use request id zero".into(),
            ));
        }
        let projection = generated_projection(message)?;
        let line = encode_mod_ipc_frame(&projection)?
            .to_json_string()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        self.send_serialized_in_epoch(epoch, request_id, line.as_bytes())
            .await
    }

    async fn send_serialized_in_epoch(
        &self,
        epoch: &Arc<ModWorkerEpoch>,
        request_id: u64,
        line: &[u8],
    ) -> Result<(), ModError> {
        let status = epoch.status.load(Ordering::Acquire);
        if status == WORKER_EPOCH_DEAD
            || (status == WORKER_EPOCH_RECOVERING
                && request_id != 0
                && !MOD_WORKER_RECOVERY_SCOPE
                    .try_with(|active| *active)
                    .unwrap_or(false))
        {
            return Err(ModError::Unavailable("Mod worker is recovering".into()));
        }
        let mut input = epoch.input.lock().await;
        let status = epoch.status.load(Ordering::Acquire);
        if status == WORKER_EPOCH_DEAD
            || (status == WORKER_EPOCH_RECOVERING
                && request_id != 0
                && !MOD_WORKER_RECOVERY_SCOPE
                    .try_with(|active| *active)
                    .unwrap_or(false))
        {
            return Err(ModError::Unavailable("Mod worker is recovering".into()));
        }
        let result = async {
            input.write_all(line).await?;
            input.write_all(b"\n").await?;
            input.flush().await
        }
        .await;
        if let Err(error) = result {
            report_worker_epoch_failure(
                &epoch,
                &self.worker_failure_sender,
                error.to_string(),
                None,
            );
            return Err(ModError::Unavailable(error.to_string()));
        }
        Ok(())
    }

    async fn request_projection(
        &self,
        message: Utf16JsonProjection,
    ) -> Result<RequestChannel, ModError> {
        self.request_projection_with_api_origin_and_session(
            message,
            lingxi_core::host::task_registry::FieldPresence::Missing,
            None,
            None,
        )
        .await
    }

    async fn request_projection_with_api_origin_and_session(
        &self,
        mut message: Utf16JsonProjection,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        generation_session: Option<Arc<dyn ModSessionContext>>,
        inherited_generation_context_ticket: Option<String>,
    ) -> Result<RequestChannel, ModError> {
        let epoch = self.current_worker_epoch().await;
        let status = epoch.status.load(Ordering::Acquire);
        if status == WORKER_EPOCH_DEAD
            || (status == WORKER_EPOCH_RECOVERING
                && !MOD_WORKER_RECOVERY_SCOPE
                    .try_with(|active| *active)
                    .unwrap_or(false))
        {
            return Err(ModError::Unavailable("Mod worker is recovering".into()));
        }
        if message.object_entries().is_none() {
            return Err(ModError::Protocol(
                "worker request must be an object".into(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id > 9_007_199_254_740_991 {
            return Err(ModError::Unavailable(
                "Mod request id exceeded JavaScript's safe integer range".into(),
            ));
        }
        let ticket = self
            .api_origin_contexts
            .issue(api_origin)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let generation_context_ticket =
            if let Some(context_ticket) = inherited_generation_context_ticket {
                if !retain_generation_api_session(&epoch.generation_api_sessions, &context_ticket) {
                    return Err(ModError::Unavailable(
                        "originating Mod dispatch generation ended".into(),
                    ));
                }
                Some(context_ticket)
            } else {
                generation_session.map(|session| {
                    // The origin ticket is stable for a captured hook origin; pair it
                    // with this worker request id so concurrent W1 generations never
                    // share the background session binding.
                    let context_ticket = format!("{}:{}:{}", self.instance_id, id, ticket.as_str());
                    epoch
                        .generation_api_sessions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(
                            context_ticket.clone(),
                            GenerationApiSessionEntry {
                                session: session.clone(),
                                references: 1,
                            },
                        );
                    if let Some(cancellation) = session.generation_cancellation_token() {
                        let sessions = epoch.generation_api_sessions.clone();
                        let ticket = context_ticket.clone();
                        tokio::spawn(async move {
                            cancellation.cancelled().await;
                            sessions
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .remove(&ticket);
                        });
                    }
                    context_ticket
                })
            };
        message
            .set_field(
                "apiContextTicket",
                projection_string_value(ticket.as_str())?,
            )
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        if let Some(context_ticket) = generation_context_ticket.as_ref() {
            message
                .set_field(
                    "generationContextTicket",
                    projection_string_value(context_ticket)?,
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        message
            .set_field("id", projection_u64_value(id)?)
            .map_err(|error| ModError::Protocol(error.to_string()))?;

        let (sender, replies) = mpsc::unbounded_channel();
        epoch
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, sender);
        let channel = RequestChannel {
            id,
            api_context_ticket: Some(ticket.as_str().to_owned()),
            generation_context_ticket,
            generation_api_sessions: epoch.generation_api_sessions.clone(),
            replies,
            routes: epoch.routes.clone(),
            input: epoch.input.clone(),
            worker_epoch: Some(epoch.clone()),
            active_request_epochs: Some(self.active_request_epochs.clone()),
            completed: false,
        };
        self.active_request_epochs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::downgrade(&epoch));
        let line = encode_mod_ipc_frame(&message)?
            .to_json_string()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        self.send_serialized(id, line.as_bytes()).await?;
        Ok(channel)
    }

    fn resolve_api_context(
        &self,
        message: &Utf16JsonProjection,
        active_request: Option<&RequestChannel>,
    ) -> Result<ResolvedModApiContext, ModError> {
        let message_id = projection_u64(message, "/id")
            .ok_or_else(|| ModError::Protocol("host API request missing request id".into()))?;
        let ticket = projection_string(message, "/apiContextTicket")
            .ok_or_else(|| ModError::Protocol("host API request missing origin ticket".into()))?;
        if let Some(request) = active_request {
            let generation_ticket = message_generation_context_ticket(message)?;
            if message_id != request.id
                || request.api_context_ticket.as_deref() != Some(ticket.as_str())
                || request.generation_context_ticket != generation_ticket
            {
                return Err(ModError::Protocol(
                    "host API request does not belong to its active dispatch".into(),
                ));
            }
        } else if message_id != 0 {
            return Err(ModError::Protocol(
                "background host API request must use request id zero".into(),
            ));
        }
        let caller = ModApiCallerKey {
            storage_id: projection_string(message, "/storageId")
                .filter(|storage_id| !storage_id.is_empty())
                .ok_or_else(|| ModError::Protocol("host API request missing storage id".into()))?,
            hook_id: projection_u64(message, "/hookId")
                .ok_or_else(|| ModError::Protocol("host API request missing hook id".into()))?,
        };
        self.api_origin_contexts
            .resolve(
                &ticket,
                &caller,
                projection_string(message, "/plugin").as_deref(),
            )
            .map_err(|error| ModError::Protocol(error.to_string()))
    }

    async fn receive(&self, channel: &mut RequestChannel) -> Result<Utf16JsonProjection, ModError> {
        match tokio::time::timeout(WORKER_RESPONSE_BUDGET, channel.replies.recv()).await {
            Ok(Some(message)) => {
                if matches!(
                    message.string_at("/kind"),
                    Some(
                        "loaded"
                            | "unloaded"
                            | "result"
                            | "error"
                            | "protocol.error"
                            | "ui.client.result"
                            | "ui.listener.result"
                            | "ui.press.preflight.result"
                    )
                ) {
                    channel.completed = true;
                }
                Ok(message)
            }
            Ok(None) => {
                if let Some(epoch) = channel.worker_epoch.as_ref() {
                    report_worker_epoch_failure(
                        epoch,
                        &self.worker_failure_sender,
                        "worker exited".into(),
                        None,
                    );
                }
                Err(ModError::Unavailable("worker exited".into()))
            }
            Err(_) => {
                let epoch = match channel.worker_epoch.as_ref() {
                    Some(epoch) => Arc::clone(epoch),
                    None => self.current_worker_epoch().await,
                };
                let _ = epoch.child.lock().await.kill().await;
                report_worker_epoch_failure(
                    &epoch,
                    &self.worker_failure_sender,
                    "worker response timed out".into(),
                    None,
                );
                Err(ModError::Timeout)
            }
        }
    }

    async fn run_store_api_core(
        &self,
        event: &str,
        input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        plugin: &str,
    ) -> Result<HostApiValue, ModError> {
        input
            .validate()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let root = self
            .store_root
            .as_ref()
            .ok_or_else(|| ModError::Unavailable("Mod store needs a config home".into()))?
            .clone();
        let plugin = plugin.to_owned();
        let event = event.to_owned();
        tokio::task::spawn_blocking(move || {
            let key = if event == "store.keys" {
                None
            } else {
                let key =
                    mod_json_string_units_at_pointer(&input, "/key").map_err(ModError::Hook)?;
                if key.len() > 256 {
                    return Err(ModError::Hook("store key exceeds 256 characters".into()));
                }
                Some(key)
            };
            let path = mod_store_file(&root, &plugin);
            let lock = if matches!(event.as_str(), "store.set" | "store.delete") {
                std::fs::create_dir_all(&root)?;
                let lock_path = path.with_extension("json.lock");
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .open(lock_path)?;
                file.lock_exclusive()?;
                Some(file)
            } else {
                None
            };
            let mut data = match std::fs::read_to_string(&path) {
                Ok(text) => lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&text)
                    .map_err(|error| ModError::Hook(format!("Mod store is malformed: {error}")))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}")
                        .map_err(|error| ModError::Hook(error.to_string()))?
                }
                Err(error) => return Err(ModError::Io(error)),
            };
            if data.object_entries().is_none() {
                return Err(ModError::Hook("Mod store is not a JSON object".into()));
            }
            let result = match event.as_str() {
                "store.get" => {
                    let value = mod_json_object_get(&data, key.as_deref().expect("store key"))
                        .map_err(ModError::Hook)?;
                    value.map_or(HostApiValue::Undefined, HostApiValue::JsonProjection)
                }
                "store.keys" => {
                    let keys = mod_json_object_keys(&data)
                        .map_err(ModError::Hook)?
                        .into_iter()
                        .map(mod_json_string_projection)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(ModError::Hook)?;
                    let projection =
                        lingxi_core::types::utf16_json::Utf16JsonProjection::array(keys)
                            .map_err(|error| ModError::Hook(error.to_string()))?;
                    HostApiValue::JsonProjection(projection)
                }
                "store.set" | "store.delete" => {
                    let key = key.expect("validated store key");
                    let changed = if event == "store.set" {
                        let value =
                            mod_exact_json_at_pointer(&input, "/value").map_err(ModError::Hook)?;
                        mod_json_object_set(&mut data, key, value).map_err(ModError::Hook)?;
                        true
                    } else {
                        mod_json_object_delete(&mut data, &key).map_err(ModError::Hook)?
                    };
                    if changed {
                        let compact = data
                            .to_json_string()
                            .map_err(|error| ModError::Hook(error.to_string()))?;
                        if compact.encode_utf16().count() > 4 * 1024 * 1024 {
                            return Err(ModError::Hook("Mod store exceeds 4 MiB of JSON".into()));
                        }
                        let mut encoded = data
                            .to_pretty_json_string()
                            .map_err(|error| ModError::Hook(error.to_string()))?;
                        encoded.push('\n');
                        let temp = path.with_extension(format!(
                            "json.tmp-{}-{}",
                            std::process::id(),
                            STORE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
                        ));
                        let write = (|| -> std::io::Result<()> {
                            let mut file = std::fs::OpenOptions::new()
                                .create_new(true)
                                .write(true)
                                .open(&temp)?;
                            file.write_all(encoded.as_bytes())?;
                            file.sync_all()?;
                            std::fs::rename(&temp, &path)
                        })();
                        if write.is_err() {
                            let _ = std::fs::remove_file(&temp);
                        }
                        write?;
                    }
                    HostApiValue::Undefined
                }
                _ => {
                    return Err(ModError::Protocol(format!(
                        "unsupported store event: {event}"
                    )));
                }
            };
            drop(lock);
            Ok(result)
        })
        .await
        .map_err(|error| ModError::Unavailable(error.to_string()))?
    }

    fn dispatch_host_api_with_projection<'a>(
        &'a self,
        event: &'a str,
        input: Option<Utf16JsonProjection>,
        cwd: &'a Path,
        session: Option<&'a dyn ModSessionContext>,
        cwd_is_pinned: bool,
        depth: u8,
        origin: Value,
        api_context: ResolvedModApiContext,
        storage_id: String,
        skip_hook_id: Option<u64>,
        log_route_id: u64,
        tool_call_context: Option<ModToolCallContext>,
        generation_context_ticket: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<HostApiValue, ModError>> + Send + 'a>> {
        self.dispatch_host_api_with_ui_render_state_read_projection(
            event,
            input,
            cwd,
            session,
            cwd_is_pinned,
            depth,
            origin,
            api_context,
            storage_id,
            skip_hook_id,
            log_route_id,
            tool_call_context,
            None,
            generation_context_ticket,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_host_api_with_ui_render_state_read_projection<'a>(
        &'a self,
        event: &'a str,
        input_projection: Option<Utf16JsonProjection>,
        cwd: &'a Path,
        session: Option<&'a dyn ModSessionContext>,
        cwd_is_pinned: bool,
        depth: u8,
        origin: Value,
        api_context: ResolvedModApiContext,
        storage_id: String,
        skip_hook_id: Option<u64>,
        log_route_id: u64,
        tool_call_context: Option<ModToolCallContext>,
        ui_render_state_read: Option<ModUiRenderStateReadScope>,
        generation_context_ticket: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<HostApiValue, ModError>> + Send + 'a>> {
        Box::pin(async move {
            let mut input_projection = input_projection;
            let input = input_projection.as_ref();
            if event == "ui.notify" && session.is_none() {
                return Err(ModError::Unavailable(
                    "ui.notify needs a session output stream".into(),
                ));
            }
            // These APIs are direct Host operations, not middleware events.
            // In particular, `agent.spawn` enters the canonical Agent launch
            // path through the session; dispatching an `agent.spawn` hook here
            // as well would run the same spawn hooks twice.
            if event == "agent.list" {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("agent.list needs a session".into()))?;
                return Ok(HostApiValue::JsonProjection(session.agent_list().await?));
            }
            if event == "agent.spawn" {
                let session = session.ok_or_else(|| {
                    ModError::Unavailable("agent.spawn needs a launch-capable session".into())
                })?;
                let spawn_input =
                    ModAgentSpawnInput::from_native_wrapper(input.ok_or_else(|| {
                        ModError::Protocol("agent.spawn input is required".into())
                    })?)?;
                let spawn_slot = self
                    .agent_spawn_budget
                    .acquire(&api_context.caller.plugin)?;
                let request_cancellation = lingxi_core::host::CancellationToken::new();
                // Dropping the API future on `api.cancel` cancels only the
                // launch/admission request. The canonical launch-only path
                // owns a separate signal for the child and its settlement.
                let _cancel_request_on_drop =
                    agent_api::CancelModAgentSpawnRequestOnDrop::new(request_cancellation.clone());
                let spawn_context = ModAgentSpawnContext::new(
                    lingxi_core::host::subagent_spawn::AgentSpawnProvenance {
                        hook_caller: lingxi_core::host::task_registry::FieldPresence::Value(
                            Value::String(api_context.caller.plugin.clone()),
                        ),
                        hook_origin: api_context.hook_origin.clone(),
                    },
                    request_cancellation,
                    spawn_slot,
                );
                return Ok(HostApiValue::JsonProjection(
                    session.agent_spawn_api(spawn_input, spawn_context).await?,
                ));
            }
            if event == "ui.invalidate"
                && input.and_then(|input| input.string_at("/event")) == Some("ui.render")
            {
                if let Some(plugin) = origin.get("plugin").and_then(Value::as_str) {
                    let update = {
                        let now_ms = self.ui_pacing_now_ms();
                        let mut state = self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let invalidated_from_live_render =
                            ui_render_state_read.as_ref().is_some_and(|scope| {
                                ModUiRenderPace::for_site(
                                    &scope.site.surface,
                                    &scope.site.component,
                                    scope.on_screen,
                                ) == ModUiRenderPace::Live
                            });
                        let active_live_site = invalidated_from_live_render
                            || state.active_live_site_for_plugin(plugin);
                        state.request_ui_render_plugin_invalidation(
                            plugin,
                            active_live_site,
                            now_ms,
                        )
                    };
                    if update.version_changed {
                        self.ui_render_generation.fetch_add(1, Ordering::AcqRel);
                    }
                    self.apply_ui_invalidation_update(update, session).await;
                }
                return Ok(HostApiValue::JsonProjection(static_projection("null")));
            }
            // `$.command.run` admits a prompt to the host queue. Its later
            // execution fires the command.run event; wrapping the admission
            // itself as that event would hand the API a `{ value }` envelope
            // and may recursively run the command before it is queued.
            // `$.ui.invalidate` changes a cache directly; it is not a hook event.
            if matches!(event, "command.run" | "prompt.submit" | "ui.invalidate") {
                let plugin = origin
                    .get("plugin")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let value = self
                    .run_host_api_core_projection(
                        event,
                        input_projection,
                        cwd,
                        session,
                        cwd_is_pinned,
                        plugin,
                        tool_call_context.as_ref(),
                        ui_render_state_read.clone(),
                    )
                    .await?;
                return Ok(value);
            }
            if matches!(event, "env.get" | "env.set") {
                let input =
                    input.ok_or_else(|| ModError::Hook(format!("{event} input is required")))?;
                let name = env_name(event, input)?;
                if event == "env.set" {
                    env_set_value(input)?;
                }
                let allowed = self
                    .env_scans
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&storage_id)
                    .is_some_and(|scan| {
                        if event == "env.get" {
                            scan.reads.contains(&name)
                        } else {
                            scan.writes.contains(&name)
                        }
                    });
                if !allowed {
                    return Err(ModError::Hook(format!(
                        "{event} refused: {name} is not named in this hooks module"
                    )));
                }
            }
            if event == "prompt.compose" {
                input_projection = session
                    .ok_or_else(|| ModError::Unavailable("prompt.compose needs a session".into()))?
                    .prompt_compose_facts(
                        input_projection.take().ok_or_else(|| {
                            ModError::Hook("prompt.compose input is required".into())
                        })?,
                    )
                    .await
                    .map(Some)?;
            }
            if matches!(event, "state.get" | "state.set") {
                let input = input_projection
                    .as_ref()
                    .ok_or_else(|| ModError::Hook(format!("{event} input is required")))?;
                let key = mod_state_key(input, event)?;
                if event == "state.set" {
                    let caller = origin
                        .get("plugin")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    if key.plugin != caller.encode_utf16().collect::<Vec<_>>() {
                        let owner = String::from_utf16_lossy(&key.plugin);
                        return Err(ModError::Native(format!(
                            "{caller}: $.state.set denied: {owner} owns that value and only its owner writes it"
                        )));
                    }
                    if input.at_pointer("/value").is_none() {
                        return Err(ModError::Hook("state.set takes a value".into()));
                    }
                    if input.at_pointer("/ifVersion").is_some()
                        && projection_u64(input, "/ifVersion")
                            .is_none_or(|version| version > 9_007_199_254_740_991)
                    {
                        return Err(ModError::Hook(
                            "state.set ifVersion must be a nonnegative safe integer".into(),
                        ));
                    }
                    let previous = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .values
                        .get(&key)
                        .map(|entry| entry.value.clone());
                    let input_projection = input_projection
                        .as_mut()
                        .expect("state.set input is present");
                    match previous {
                        Some(value) => set_projection_field(input_projection, "previous", value)?,
                        None => {
                            input_projection
                                .remove_field("previous")
                                .map_err(|error| ModError::Protocol(error.to_string()))?;
                        }
                    }
                }
            }
            let requested_bytes = event == "fs.read"
                && input_projection
                    .as_ref()
                    .and_then(|input| input.string_at("/as"))
                    == Some("bytes");
            let live_cwd = if cwd_is_pinned {
                cwd.to_path_buf()
            } else {
                session.map_or_else(|| cwd.to_path_buf(), ModSessionContext::cwd)
            };
            let generation_session = session.and_then(|session| session.ui_invalidation_context());
            let mut dispatch_message = generated_projection(
                &json!({"kind":"dispatch","event":event,"cwd":live_cwd,"origin":origin,"skipHookId":skip_hook_id,"logRouteId":log_route_id,
                "secDefaultOrder":self.sec_default_order.load(Ordering::Acquire)}),
            )?;
            if let Some(input) = input_projection.clone() {
                set_projection_field(&mut dispatch_message, "input", input)?;
            }
            let mut request = self
                .request_projection_with_api_origin_and_session(
                    dispatch_message,
                    api_context.hook_origin.clone(),
                    generation_session,
                    generation_context_ticket.clone(),
                )
                .await?;
            type ClockCall<'a> = Pin<
                Box<dyn Future<Output = (u64, Option<Result<HostApiValue, ModError>>)> + Send + 'a>,
            >;
            let mut clock_calls: FuturesUnordered<ClockCall<'_>> = FuturesUnordered::new();
            let mut clock_abort = HashMap::<u64, ApiCallControl>::new();
            loop {
                let message = if clock_calls.is_empty() {
                    self.receive(&mut request).await?
                } else {
                    tokio::select! {
                        completed = clock_calls.next() => {
                            if let Some((call_id, result)) = completed {
                                let cancelled_without_result =
                                    tool_call_cancelled_without_result(
                                        clock_abort.get(&call_id),
                                        &result,
                                    );
                                clock_abort.remove(&call_id);
                                if cancelled_without_result {
                                    self.send_generated(&json!({
                                        "id":request.id,
                                        "kind":"api.error",
                                        "callId":call_id,
                                        "message":TOOL_CALL_ABORTED_MESSAGE
                                    })).await?;
                                } else {
                                    match result {
                                        Some(Ok(value)) => {
                                            let reply = value.reply(request.id, call_id)?;
                                            self.send_projection(&reply).await?;
                                        }
                                        Some(Err(error)) => self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?,
                                        None => {},
                                    }
                                }
                            }
                            continue;
                        }
                        reply = request.replies.recv() => reply.ok_or_else(|| ModError::Unavailable("worker exited".into()))?,
                    }
                };
                match message.string_at("/kind") {
                    Some("next") => {
                        let call_id = projection_u64(&message, "/callId").ok_or_else(|| {
                            ModError::Protocol("host API next missing callId".into())
                        })?;
                        let next_projection = message
                            .at_pointer("/input")
                            .map(|_| message.subprojection("/input"))
                            .transpose()
                            .map_err(|error| ModError::Protocol(error.to_string()))?;
                        if event == "tool.check"
                            && !same_projection_field_opt(&input_projection, &next_projection, "")
                        {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"tool.check identity is pinned"})).await?;
                            continue;
                        }
                        if event == "tool.call"
                            && ["tool", "tool_use_id", "agentId", "consent", "$shadowed"]
                                .iter()
                                .any(|field| {
                                    let pointer = format!("/{field}");
                                    !same_projection_field_opt(
                                        &input_projection,
                                        &next_projection,
                                        &pointer,
                                    )
                                })
                        {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"tool.call identity and consent are pinned"})).await?;
                            continue;
                        }
                        if event == "prompt.compose"
                            && !same_projection_field_opt(
                                &input_projection,
                                &next_projection,
                                "/model",
                            )
                        {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"prompt.compose model is pinned"})).await?;
                            continue;
                        }
                        if matches!(event, "env.get" | "env.set")
                            && !same_projection_field_opt(
                                &input_projection,
                                &next_projection,
                                "/name",
                            )
                        {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":format!("{event} name is pinned")})).await?;
                            continue;
                        }
                        if event == "session.receive"
                            && ["origin", "event", "agentId"].iter().any(|field| {
                                !same_projection_field_opt(
                                    &input_projection,
                                    &next_projection,
                                    &format!("/{field}"),
                                )
                            })
                        {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"session.receive origin and event are pinned"})).await?;
                            continue;
                        }
                        if matches!(event, "state.get" | "state.set")
                            && (next_projection
                                .as_ref()
                                .and_then(|input| mod_state_key(input, event).ok())
                                != input_projection
                                    .as_ref()
                                    .and_then(|input| mod_state_key(input, event).ok()))
                        {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"state reference is pinned"})).await?;
                            continue;
                        }
                        if event == "state.set"
                            && (!same_projection_field_opt(
                                &input_projection,
                                &next_projection,
                                "/ifVersion",
                            ) || !same_projection_field_opt(
                                &input_projection,
                                &next_projection,
                                "/previous",
                            ))
                        {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"state.set ifVersion and previous are pinned"})).await?;
                            continue;
                        }
                        let core = if event.starts_with("store.") {
                            self.run_store_api_core(
                                event,
                                next_projection.ok_or_else(|| {
                                    ModError::Hook(format!("{event} input is required"))
                                })?,
                                &storage_id,
                            )
                            .await
                        } else if event == "prompt.compose" {
                            let session = session.ok_or_else(|| {
                                ModError::Unavailable("prompt.compose needs a session".into())
                            })?;
                            session
                                .prompt_compose_core(
                                    next_projection.ok_or_else(|| {
                                        ModError::Hook("prompt.compose input is required".into())
                                    })?,
                                    Some(origin.clone()),
                                    skip_hook_id,
                                )
                                .await
                                .map(HostApiValue::JsonProjection)
                        } else {
                            self.run_host_api_core_projection(
                                event,
                                next_projection,
                                cwd,
                                session,
                                cwd_is_pinned,
                                origin
                                    .get("plugin")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown"),
                                tool_call_context.as_ref(),
                                ui_render_state_read.clone(),
                            )
                            .await
                            .map(|value| match value {
                                HostApiValue::JsonProjection(value)
                                    if matches!(
                                        event,
                                        "env.get"
                                            | "ui.selection"
                                            | "ui.copy"
                                            | "prompt.read"
                                            | "prompt.fill"
                                            | "prompt.suggest"
                                            | "model.classify"
                                            | "telemetry.log"
                                            | "telemetry.mark"
                                    ) && value
                                        .at_pointer("")
                                        .is_some_and(JsonValueView::is_null) =>
                                {
                                    HostApiValue::Undefined
                                }
                                value => value,
                            })
                        };
                        match core {
                            Ok(result) => {
                                let is_structured_event = matches!(
                                    event,
                                    "tool.check"
                                        | "tool.call"
                                        | "prompt.compose"
                                        | "prompt.fill"
                                        | "prompt.suggest"
                                        | "prompt.context"
                                        | "session.receive"
                                );
                                let result = match result {
                                    HostApiValue::JsonProjection(value) if is_structured_event => {
                                        value
                                    }
                                    HostApiValue::JsonProjection(value) => {
                                        Utf16JsonProjection::object(vec![("value".into(), value)])
                                            .map_err(|error| ModError::Protocol(error.to_string()))?
                                    }
                                    HostApiValue::Undefined if is_structured_event => {
                                        return Err(ModError::Protocol(format!(
                                            "{event} core returned undefined"
                                        )));
                                    }
                                    HostApiValue::Undefined => static_projection("{}"),
                                };
                                let mut reply = generated_projection(&json!({
                                    "id":request.id,
                                    "kind":"next.result",
                                    "callId":call_id,
                                }))?;
                                set_projection_field(&mut reply, "result", result)?;
                                self.send_projection(&reply).await?;
                            }
                            Err(error) => {
                                self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":error.to_string()}))
                                .await?;
                            }
                        }
                    }
                    Some("result") => {
                        request.completed = true;
                        let result_projection = message
                            .subprojection("/result")
                            .map_err(|error| ModError::Protocol(error.to_string()))?;
                        let result = result_projection.clone();
                        if event == "tool.check" {
                            if !valid_tool_check_result(&result) {
                                return Err(ModError::Hook(
                                    "tool.check result must contain decision allow, ask, or deny"
                                        .into(),
                                ));
                            }
                            return Ok(HostApiValue::JsonProjection(result_projection));
                        }
                        if event == "prompt.fill" {
                            // The Native fill operation reports the text and
                            // cursor after applying the fill. Read them through
                            // the same host UI route so hook rewrites and exact
                            // UTF-16 source remain visible to the caller.
                            let after = self
                                .dispatch_host_api_with_projection(
                                    "prompt.read",
                                    Some(static_projection("{}")),
                                    cwd,
                                    session,
                                    cwd_is_pinned,
                                    depth.saturating_add(1),
                                    origin.clone(),
                                    api_context.clone(),
                                    storage_id.clone(),
                                    skip_hook_id,
                                    log_route_id,
                                    tool_call_context.clone(),
                                    generation_context_ticket.clone(),
                                )
                                .await?;
                            let after = match after {
                                HostApiValue::JsonProjection(value) => value,
                                HostApiValue::Undefined => {
                                    static_projection(r#"{"text":"","cursor":0}"#)
                                }
                            };
                            let text = after
                                .subprojection("/text")
                                .unwrap_or_else(|_| static_projection(r#""""#));
                            let cursor = after
                                .subprojection("/cursor")
                                .unwrap_or_else(|_| static_projection("0"));
                            let mut answer = result_projection;
                            set_projection_field(&mut answer, "text", text)?;
                            set_projection_field(&mut answer, "cursor", cursor)?;
                            return Ok(HostApiValue::JsonProjection(answer));
                        }
                        if event == "prompt.suggest" {
                            return Ok(HostApiValue::JsonProjection(result_projection));
                        }
                        if event == "prompt.compose" {
                            if !valid_prompt_compose_result(&result_projection) {
                                return Err(ModError::Hook(
                                    "prompt.compose result must contain ordered, unique sections"
                                        .into(),
                                ));
                            }
                            return Ok(HostApiValue::JsonProjection(result_projection));
                        }
                        if event == "prompt.context" {
                            if !valid_prompt_context_result(&result_projection) {
                                return Err(ModError::Hook(
                                    "prompt.context result must contain valid blocks".into(),
                                ));
                            }
                            return Ok(HostApiValue::JsonProjection(result_projection));
                        }
                        if event == "session.receive" {
                            let valid = result
                                .at_pointer("")
                                .and_then(JsonValueView::object_entries)
                                .is_some()
                                && ((result.string_at("/text").is_some()
                                    && result.at_pointer("/consumed").is_none())
                                    || (result.string_at("/consumed").is_some()
                                        && result.at_pointer("/text").is_none()));
                            if !valid {
                                return Err(ModError::Hook(
                                    "session.receive result must contain text or consumed".into(),
                                ));
                            }
                            return Ok(HostApiValue::JsonProjection(result_projection));
                        }
                        if event == "tool.call" {
                            if !valid_tool_call_middleware_answer(&result) {
                                return Err(ModError::Hook(
                                    "tool.call result must contain result or deny".into(),
                                ));
                            }
                            let session = session.ok_or_else(|| {
                                ModError::Unavailable("tool.call needs a session".into())
                            })?;
                            let context = tool_call_context.as_ref().ok_or_else(|| {
                                ModError::Protocol(
                                    "tool.call API request is missing its host context".into(),
                                )
                            })?;
                            let input = input_projection.as_ref().ok_or_else(|| {
                                ModError::Protocol("tool.call input is missing".into())
                            })?;
                            let projected = session
                                .project_tool_call_api_result(
                                    input.clone(),
                                    result.clone(),
                                    context,
                                )
                                .await?;
                            if !valid_tool_call_api_projection(&projected) {
                                return Err(ModError::Protocol(
                                    "tool.call API projection returned an unsupported result"
                                        .into(),
                                ));
                            }
                            complete_tool_call(session, context).await?;
                            if projected.semantically_eq(&result) {
                                return Ok(HostApiValue::JsonProjection(result_projection));
                            }
                            return Ok(HostApiValue::JsonProjection(projected));
                        }
                        if let Some(reason) = result.string_at("/deny") {
                            if matches!(event, "telemetry.log" | "telemetry.mark") {
                                let plugin = origin
                                    .get("plugin")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                return Err(ModError::Native(format!(
                                    "{plugin}: $.{event}: {reason}"
                                )));
                            }
                            return Err(ModError::Hook(reason.into()));
                        }
                        if matches!(
                            event,
                            "store.get"
                                | "env.get"
                                | "ui.selection"
                                | "model.classify"
                                | "telemetry.log"
                                | "telemetry.mark"
                        ) && result
                            .at_pointer("")
                            .and_then(JsonValueView::object_entries)
                            .is_some()
                            && result.at_pointer("/value").is_none()
                        {
                            return Ok(HostApiValue::Undefined);
                        }
                        // JavaScript's `{ value: undefined }` becomes `{}` on the
                        // worker's JSON wire. Only void operations may use it.
                        let value = match result.subprojection("/value") {
                            Ok(value) => value,
                            Err(_)
                                if matches!(
                                    event,
                                    "fs.write"
                                        | "ui.log"
                                        | "ui.toast"
                                        | "ui.notify"
                                        | "ui.status"
                                        | "clock.sleep"
                                        | "clock.after"
                                        | "clock.every"
                                        | "store.set"
                                        | "store.delete"
                                        | "env.set"
                                        | "telemetry.log"
                                        | "telemetry.mark"
                                ) && result
                                    .at_pointer("")
                                    .and_then(JsonValueView::object_entries)
                                    .is_some() =>
                            {
                                static_projection("null")
                            }
                            Err(_) => {
                                return Err(ModError::Hook(format!(
                                    "{event} result must contain value or deny"
                                )));
                            }
                        };
                        match event {
                            "clock.now" if value.number_at("").is_none() => {
                                return Err(ModError::Hook(
                                    "clock.now result must be a number".into(),
                                ));
                            }
                            "session.cwd" | "session.root" | "session.model" | "session.id"
                                if value.string_at("").is_none() =>
                            {
                                return Err(ModError::Hook(format!(
                                    "{event} result must be a string"
                                )));
                            }
                            "session.turns" if projection_u64(&value, "").is_none() => {
                                return Err(ModError::Hook(
                                    "session.turns result must be a nonnegative integer".into(),
                                ));
                            }
                            "session.surfaces"
                                if !value.array_len("").is_some_and(|count| {
                                    (0..count).all(|index| {
                                        matches!(
                                            value.string_at(&format!("/{index}")),
                                            Some("terminal" | "desktop" | "mobile" | "vscode")
                                        )
                                    })
                                }) =>
                            {
                                return Err(ModError::Hook(
                                    "session.surfaces result must be an array of render surfaces"
                                        .into(),
                                ));
                            }
                            "session.surface"
                                if value
                                    .at_pointer("")
                                    .is_none_or(|surface| !surface.is_null())
                                    && !matches!(
                                        value.string_at(""),
                                        Some("terminal" | "desktop" | "mobile" | "vscode")
                                    ) =>
                            {
                                return Err(ModError::Hook(
                                    "session.surface result must be a render surface or null"
                                        .into(),
                                ));
                            }
                            "settings.read"
                                if value
                                    .at_pointer("")
                                    .and_then(JsonValueView::object_entries)
                                    .is_none() =>
                            {
                                return Err(ModError::Hook(
                                    "settings.read result must be an object".into(),
                                ));
                            }
                            "tool.list" if !valid_tool_list_result(&value) => {
                                return Err(ModError::Hook(
                                    "tool.list result must be an array of tools".into(),
                                ));
                            }
                            "command.list" if !valid_command_list_result(&value) => {
                                return Err(ModError::Hook(
                                    "command.list result must be an array of commands".into(),
                                ));
                            }
                            "command.register" if value.string_at("/command").is_none() => {
                                return Err(ModError::Hook(
                                    "command.register result must contain command".into(),
                                ));
                            }
                            "tool.register" if value.string_at("/tool").is_none() => {
                                return Err(ModError::Hook(
                                    "tool.register result must contain tool".into(),
                                ));
                            }
                            "session.messages" if value.array_len("").is_none() => {
                                return Err(ModError::Hook(
                                    "session.messages result must be an array".into(),
                                ));
                            }
                            "session.usage" if !valid_session_usage_result(&value) => {
                                return Err(ModError::Hook(
                                    "session.usage result must contain startedAt, context, rateLimits, and cost".into(),
                                ));
                            }
                            "model.fork" if !valid_model_fork_result(&value) => {
                                return Err(ModError::Hook(
                                    "model.fork result must contain isAnswered and a valid answer or reason".into(),
                                ));
                            }
                            "model.complete" if !valid_model_complete_result(&value) => {
                                return Err(ModError::Hook(
                                    "model.complete result must contain isAnswered, usage, and a valid answer or reason".into(),
                                ));
                            }
                            "model.classify" if value.string_at("").is_none() => {
                                return Err(ModError::Hook(
                                    "model.classify result must be a string or undefined".into(),
                                ));
                            }
                            "state.get" if projection_u64(&value, "/version").is_none() => {
                                return Err(ModError::Hook(
                                    "state.get result must contain a version".into(),
                                ));
                            }
                            "state.set"
                                if value
                                    .at_pointer("/isSet")
                                    .and_then(JsonValueView::as_bool)
                                    .is_none()
                                    || projection_u64(&value, "/version").is_none() =>
                            {
                                return Err(ModError::Hook(
                                    "state.set result must contain isSet and version".into(),
                                ));
                            }
                            "env.get" if value.string_at("").is_none() => {
                                return Err(ModError::Hook(
                                    "env.get result must be text or undefined".into(),
                                ));
                            }
                            "session.repo"
                                if value.at_pointer("").is_none_or(|repo| !repo.is_null())
                                    && (value
                                        .at_pointer("")
                                        .and_then(JsonValueView::object_entries)
                                        .is_none()
                                        || value.string_at("/root").is_none()
                                        || !value.at_pointer("/remote").is_some_and(
                                            |remote| remote.is_null() || remote.as_str().is_some(),
                                        )
                                        || value
                                            .at_pointer("/internal")
                                            .and_then(JsonValueView::as_bool)
                                            .is_none()
                                        || !value.at_pointer("/name").is_some_and(|name| {
                                            name.is_null() || name.as_str().is_some()
                                        })) =>
                            {
                                return Err(ModError::Hook(
                                    "session.repo result must be a repository or null".into(),
                                ));
                            }
                            "fs.read"
                                if (requested_bytes && value.string_at("/base64").is_none())
                                    || (!requested_bytes && value.string_at("").is_none()) =>
                            {
                                return Err(ModError::Hook(
                                    "fs.read result must be text or { base64 }".into(),
                                ));
                            }
                            "fs.write"
                                if value.at_pointer("").is_none_or(|root| !root.is_null()) =>
                            {
                                return Err(ModError::Hook("fs.write result must be void".into()));
                            }
                            "ui.log" | "ui.toast" | "ui.notify" | "ui.status" | "telemetry.log"
                            | "telemetry.mark" | "clock.sleep" | "clock.after" | "clock.every"
                            | "store.set" | "store.delete"
                                if value.at_pointer("").is_none_or(|root| !root.is_null()) =>
                            {
                                return Err(ModError::Hook(format!("{event} result must be void")));
                            }
                            "fs.exists"
                                if value
                                    .at_pointer("")
                                    .and_then(JsonValueView::as_bool)
                                    .is_none() =>
                            {
                                return Err(ModError::Hook(
                                    "fs.exists result must be boolean".into(),
                                ));
                            }
                            "fs.list" if value.array_len("").is_none() => {
                                return Err(ModError::Hook(
                                    "fs.list result must be an array".into(),
                                ));
                            }
                            "fs.ancestors" if value.array_len("").is_none() => {
                                return Err(ModError::Hook(
                                    "fs.ancestors result must be an array".into(),
                                ));
                            }
                            "fs.stat"
                                if value
                                    .at_pointer("")
                                    .and_then(JsonValueView::object_entries)
                                    .is_none() =>
                            {
                                return Err(ModError::Hook(
                                    "fs.stat result must be an object".into(),
                                ));
                            }
                            "process.run"
                                if value
                                    .at_pointer("")
                                    .and_then(JsonValueView::object_entries)
                                    .is_none()
                                    || projection_i64(&value, "/exitCode").is_none()
                                    || value.string_at("/stdout").is_none()
                                    || value.string_at("/stderr").is_none() =>
                            {
                                return Err(ModError::Hook(
                                    "process.run result must contain exitCode, stdout, and stderr"
                                        .into(),
                                ));
                            }
                            "store.keys"
                                if value.array_len("").is_none_or(|count| {
                                    (0..count).any(|index| {
                                        value.string_at(&format!("/{index}")).is_none()
                                    })
                                }) =>
                            {
                                return Err(ModError::Hook(
                                    "store.keys result must be strings".into(),
                                ));
                            }
                            _ => {}
                        }
                        if matches!(
                            event,
                            "state.get" | "store.get" | "store.keys" | "model.classify"
                        ) {
                            return result_projection
                                .subprojection("/value")
                                .map(HostApiValue::JsonProjection)
                                .map_err(|error| ModError::Protocol(error.to_string()));
                        }
                        if event == "session.messages" {
                            return Ok(HostApiValue::JsonProjection(value));
                        }
                        return Ok(HostApiValue::JsonProjection(value));
                    }
                    Some("log") => {
                        let plugin = message.string_at("/plugin").unwrap_or("unknown");
                        let line = message.string_at("/text").unwrap_or("");
                        if message.string_at("/to") == Some("debug") {
                            tracing::debug!(plugin, text = line, "mod log");
                        } else {
                            tracing::info!(plugin, text = line, "mod log");
                        }
                    }
                    Some("log.error") => {
                        tracing::warn!(message = %message.to_json_string().unwrap_or_default(), "mod log call dropped");
                    }
                    Some("progress") => {}
                    Some("api") => {
                        let call_id = projection_u64(&message, "/callId").ok_or_else(|| {
                            ModError::Protocol("host API request missing callId".into())
                        })?;
                        let api_context = match self.resolve_api_context(&message, Some(&request)) {
                            Ok(context) => context,
                            Err(error) => {
                                self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?;
                                continue;
                            }
                        };
                        let method = message.string_at("/method").unwrap_or("");
                        if matches!(
                            method,
                            "clock.sleep"
                                | "process.run"
                                | "model.complete"
                                | "tool.call"
                                | "agent.spawn"
                        ) && depth < 16
                        {
                            let (abort, registration) = AbortHandle::new_pair();
                            let cancellation = lingxi_core::host::CancellationToken::new();
                            let control = ApiCallControl::new(
                                abort,
                                cancellation.clone(),
                                method == "tool.call",
                            );
                            clock_abort.insert(call_id, control.clone());
                            let input_projection = message
                                .at_pointer("/input")
                                .map(|_| message.subprojection("/input"))
                                .transpose()
                                .map_err(|error| ModError::Protocol(error.to_string()))?;
                            if method == "tool.call" && input_projection.is_none() {
                                self.send_generated(&json!({
                                    "id":request.id,
                                    "kind":"api.error",
                                    "callId":call_id,
                                    "message":"tool.call requires input"
                                }))
                                .await?;
                                continue;
                            }
                            let input_for_tool = input_projection.clone();
                            let hook_id = projection_u64(&message, "/hookId");
                            let method = method.to_owned();
                            let api_context_for_call = api_context.clone();
                            let origin = json!({
                                "plugin":api_context.caller.plugin,
                                "tier":message.string_at("/tier").unwrap_or("user")
                            });
                            let storage_id = message
                                .string_at("/storageId")
                                .unwrap_or("unknown")
                                .to_owned();
                            let generation_ticket = message
                                .string_at("/generationContextTicket")
                                .map(str::to_owned);
                            clock_calls.push(Box::pin(async move {
                                let context_slot = Arc::new(StdMutex::new(None));
                                let context_for_call = context_slot.clone();
                                let execution = Abortable::new(
                                    async {
                                        let tool_call_context = if method == "tool.call" {
                                            let input = input_for_tool
                                                .as_ref()
                                                .expect("tool.call input validated above");
                                            match acquire_tool_call_context(
                                                session,
                                                cancellation.clone(),
                                                input,
                                                &api_context_for_call,
                                            )
                                            .await
                                            {
                                                Ok(context) => Some(context),
                                                Err(error) => return (Err(error), None),
                                            }
                                        } else {
                                            None
                                        };
                                        *context_for_call
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                            tool_call_context.clone();
                                        let _cancel_tool_call_on_drop =
                                            CancelToolCallOnDrop::new(tool_call_context.as_ref());
                                        let result = self
                                            .dispatch_host_api_with_projection(
                                                &method,
                                                input_projection,
                                                cwd,
                                                session,
                                                cwd_is_pinned,
                                                depth + 1,
                                                origin,
                                                api_context_for_call,
                                                message
                                                    .string_at("/storageId")
                                                    .unwrap_or("unknown")
                                                    .to_owned(),
                                                hook_id,
                                                log_route_id,
                                                tool_call_context.clone(),
                                                message
                                                    .string_at("/generationContextTicket")
                                                    .map(str::to_owned),
                                            )
                                            .await;
                                        (result, tool_call_context)
                                    },
                                    registration,
                                )
                                .await
                                .ok();
                                let completed = execution.is_some();
                                let result = execution.map(|(result, _context)| result);
                                let tool_call_context = context_slot
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .clone();
                                if let (Some(context), Some(session)) =
                                    (tool_call_context.as_ref(), session)
                                {
                                    if result.as_ref().is_none_or(Result::is_err) {
                                        let _ = abort_tool_call(session, context).await;
                                    }
                                }
                                if completed {
                                    control.finish();
                                }
                                (call_id, result)
                            }));
                            continue;
                        }
                        let result = if depth >= 16 {
                            Err(ModError::Hook("Mod API nesting limit exceeded".into()))
                        } else if matches!(
                            method,
                            "ui.invalidate"
                                | "prompt.compose"
                                | "prompt.context"
                                | "ui.log"
                                | "ui.toast"
                                | "ui.notify"
                                | "ui.status"
                                | "ui.selection"
                                | "telemetry.log"
                                | "telemetry.mark"
                                | "tool.call"
                                | "tool.check"
                                | "tool.list"
                                | "agent.list"
                                | "agent.spawn"
                                | "command.list"
                                | "command.register"
                                | "command.run"
                                | "prompt.submit"
                                | "tool.register"
                                | "clock.now"
                                | "clock.sleep"
                                | "clock.after"
                                | "clock.every"
                                | "process.run"
                                | "env.get"
                                | "env.set"
                                | "session.cwd"
                                | "session.root"
                                | "session.model"
                                | "session.id"
                                | "session.turns"
                                | "session.repo"
                                | "session.version"
                                | "session.receive"
                                | "session.messages"
                                | "session.usage"
                                | "session.surfaces"
                                | "session.surface"
                                | "model.fork"
                                | "model.complete"
                                | "model.classify"
                                | "settings.read"
                                | "fs.read"
                                | "fs.write"
                                | "fs.exists"
                                | "fs.list"
                                | "fs.stat"
                                | "fs.ancestors"
                                | "store.get"
                                | "store.set"
                                | "store.delete"
                                | "store.keys"
                                | "state.get"
                                | "state.set"
                        ) {
                            let api_context = api_context.clone();
                            let caller_origin = json!({
                                "plugin":api_context.caller.plugin,
                                "tier":message.string_at("/tier").unwrap_or("user")
                            });
                            let input_projection = message
                                .at_pointer("/input")
                                .map(|_| message.subprojection("/input"))
                                .transpose()
                                .map_err(|error| ModError::Protocol(error.to_string()))?;
                            Box::pin(
                                self.dispatch_host_api_with_projection(
                                    method,
                                    input_projection,
                                    cwd,
                                    session,
                                    cwd_is_pinned,
                                    depth + 1,
                                    caller_origin,
                                    api_context,
                                    message
                                        .string_at("/storageId")
                                        .unwrap_or("unknown")
                                        .to_owned(),
                                    projection_u64(&message, "/hookId"),
                                    log_route_id,
                                    None,
                                    message
                                        .string_at("/generationContextTicket")
                                        .map(str::to_owned),
                                ),
                            )
                            .await
                        } else {
                            Err(ModError::Protocol(format!(
                                "unsupported Mod API method: {method}"
                            )))
                        };
                        match result {
                            Ok(result) => {
                                let reply = result.reply(request.id, call_id)?;
                                self.send_projection(&reply).await?;
                            }
                            Err(error) => {
                                self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?;
                            }
                        }
                    }
                    Some("api.cancel") => {
                        if let Some(call_id) = projection_u64(&message, "/callId") {
                            if let Some(control) = clock_abort.get(&call_id) {
                                control.cancel();
                            }
                        }
                    }
                    Some("error") => {
                        return Err(ModError::Hook(
                            message
                                .string_at("/message")
                                .unwrap_or("unknown error")
                                .into(),
                        ));
                    }
                    _ => {
                        return Err(ModError::Protocol(format!(
                            "unexpected host API reply: {}",
                            message.to_json_string().unwrap_or_default()
                        )));
                    }
                }
            }
        })
    }

    async fn run_host_api_core_projection(
        &self,
        event: &str,
        input: Option<Utf16JsonProjection>,
        cwd: &Path,
        session: Option<&dyn ModSessionContext>,
        cwd_is_pinned: bool,
        plugin: &str,
        tool_call_context: Option<&ModToolCallContext>,
        ui_render_state_read: Option<ModUiRenderStateReadScope>,
    ) -> Result<HostApiValue, ModError> {
        if let Some(input) = input.as_ref() {
            input
                .validate()
                .map_err(|error| ModError::Protocol(error.to_string()))?;
        }
        if event == "state.get" {
            let input = input
                .as_ref()
                .ok_or_else(|| ModError::Hook("state.get requires JSON input".into()))?;
            let key = mod_state_key(input, event)?;
            let (entry, version) = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let entry = state.values.get(&key).cloned();
                let version = entry.as_ref().map_or(0, |entry| entry.version);
                if let Some(scope) = ui_render_state_read.as_ref() {
                    state.note_ui_render_read(scope, key, version);
                }
                (entry, version)
            };
            let version = projection_u64_value(version)?;
            let result = if let Some(entry) = entry {
                projection_object(vec![
                    ("value".into(), entry.value),
                    ("version".into(), version),
                ])?
            } else {
                projection_object(vec![("version".into(), version)])?
            };
            return Ok(HostApiValue::JsonProjection(result));
        }
        if event == "state.set" {
            let input = input
                .as_ref()
                .ok_or_else(|| ModError::Hook("state.set requires JSON input".into()))?;
            let key = mod_state_key(input, event)?;
            if key.plugin != plugin.encode_utf16().collect::<Vec<_>>() {
                let owner = String::from_utf16_lossy(&key.plugin);
                return Err(ModError::Native(format!(
                    "{plugin}: $.state.set denied: {owner} owns that value and only its owner writes it"
                )));
            }
            let value = mod_exact_json_at_pointer(input, "/value").map_err(ModError::Hook)?;
            if value
                .to_json_string()
                .map_err(|error| ModError::Hook(error.to_string()))?
                .encode_utf16()
                .count()
                > 4 * 1024 * 1024
            {
                return Err(ModError::Hook("state.set value exceeds 4 MiB".into()));
            }
            let expected = match input
                .at_pointer("")
                .and_then(|args| args.get_property("ifVersion"))
            {
                Some(value) => {
                    let number = value
                        .as_number()
                        .filter(|version| {
                            version.is_finite()
                                && *version >= 0.0
                                && version.fract() == 0.0
                                && *version <= 9_007_199_254_740_991.0
                        })
                        .ok_or_else(|| {
                            ModError::Hook(
                                "state.set ifVersion must be a nonnegative safe integer".into(),
                            )
                        })?;
                    Some(number as u64)
                }
                None => None,
            };
            let (version, did_set, update) = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let current = state.values.get(&key).map_or(0, |entry| entry.version);
                if expected.is_some_and(|expected| expected != current) {
                    let update = state.flush_ui_render_state_folds(self.ui_pacing_now_ms());
                    (current, false, update)
                } else {
                    let version = current.max(state.version_floor).saturating_add(1);
                    state
                        .values
                        .insert(key.clone(), ModStateValue { value, version });
                    let now_ms = self.ui_pacing_now_ms();
                    let mut update = state.schedule_ui_render_write(&key, now_ms);
                    update.append(state.flush_ui_render_state_folds(now_ms));
                    update.timers.clear();
                    (version, true, update)
                }
            };
            self.apply_ui_invalidation_update(update, session).await;
            let result = generated_projection(&json!({"isSet":did_set,"version":version}))?;
            return Ok(HostApiValue::JsonProjection(result));
        }
        if event.starts_with("store.") {
            let input =
                input.ok_or_else(|| ModError::Hook(format!("{event} requires JSON input")))?;
            return self.run_store_api_core(event, input, plugin).await;
        }
        if event == "session.messages" {
            let session = session
                .ok_or_else(|| ModError::Unavailable("session.messages needs a session".into()))?;
            return session
                .messages(input)
                .await
                .map(HostApiValue::JsonProjection);
        }
        if event == "session.usage" {
            let session = session
                .ok_or_else(|| ModError::Unavailable("session.usage needs a session".into()))?;
            return session.usage(input).await.map(HostApiValue::JsonProjection);
        }
        if event == "model.classify" {
            let session = session
                .ok_or_else(|| ModError::Unavailable("model.classify needs a session".into()))?;
            let input =
                input.ok_or_else(|| ModError::Hook("model.classify input is required".into()))?;
            let result = session.model_classify(input, plugin).await?;
            result
                .validate()
                .map_err(|error| ModError::Protocol(error.to_string()))?;
            if result.at_pointer("").is_some_and(JsonValueView::is_null) {
                return Ok(HostApiValue::Undefined);
            }
            if result.string_units("").is_some() {
                return Ok(HostApiValue::JsonProjection(result));
            }
            return Err(ModError::Hook(
                "model.classify result must be a string or undefined".into(),
            ));
        }
        if event == "prompt.context" {
            return input
                .map(HostApiValue::JsonProjection)
                .ok_or_else(|| ModError::Hook("prompt.context input is required".into()));
        }
        if event == "prompt.compose" {
            let session = session
                .ok_or_else(|| ModError::Unavailable("prompt.compose needs a session".into()))?;
            let facts =
                input.ok_or_else(|| ModError::Hook("prompt.compose input is required".into()))?;
            return session
                .prompt_compose_core(facts, None, None)
                .await
                .map(HostApiValue::JsonProjection);
        }
        if event == "env.get" {
            let input = input
                .as_ref()
                .ok_or_else(|| ModError::Hook("env.get input is required".into()))?;
            let name = env_name(event, input)?;
            if name == "USER_TYPE" {
                return Ok(HostApiValue::JsonProjection(root_string_projection(
                    "external",
                )));
            }
            let override_value = self
                .environment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&name)
                .cloned();
            return match override_value {
                Some(Some(value)) => {
                    Ok(HostApiValue::JsonProjection(root_string_projection(&value)))
                }
                Some(None) => Ok(HostApiValue::Undefined),
                None => match std::env::var_os(&name) {
                    Some(value) => Ok(HostApiValue::JsonProjection(root_string_projection(
                        &value.to_string_lossy(),
                    ))),
                    None => Ok(HostApiValue::Undefined),
                },
            };
        }
        if matches!(
            event,
            "ui.selection" | "ui.copy" | "prompt.read" | "prompt.fill" | "prompt.suggest"
        ) {
            let Some(session) = session else {
                return Ok(HostApiValue::Undefined);
            };
            let input = input.unwrap_or_else(|| static_projection("{}"));
            let result = session.remote_ui_operation(event, input, plugin).await?;
            if result.at_pointer("").is_some_and(JsonValueView::is_null) {
                return Ok(HostApiValue::Undefined);
            }
            return Ok(HostApiValue::JsonProjection(result));
        }

        let live_cwd = if cwd_is_pinned {
            cwd.to_path_buf()
        } else {
            session.map_or_else(|| cwd.to_path_buf(), ModSessionContext::cwd)
        };
        let result = match event {
            // Mod-origin telemetry methods are hook-only. The ordinary
            // collector telemetry site has a different core.
            "telemetry.log" | "telemetry.mark" => static_projection("null"),
            "model.fork" => {
                let input =
                    input.ok_or_else(|| ModError::Hook("model.fork input is required".into()))?;
                let prompt = input
                    .string_units("/prompt")
                    .ok_or_else(|| ModError::Hook("model.fork prompt must be a string".into()))?;
                if js_trimmed_units(&prompt).is_empty() {
                    return Err(ModError::Hook(
                        "model.fork prompt must be a non-empty string".into(),
                    ));
                }
                let prompt = input
                    .subprojection("/prompt")
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                let fork_input = projection_object(vec![("prompt".into(), prompt)])?;
                match session {
                    Some(session) => session.model_fork(fork_input).await?,
                    None => static_projection(r#"{"isAnswered":false,"reason":"nothing-to-fork"}"#),
                }
            }
            "model.complete" => {
                let session = session.ok_or_else(|| {
                    ModError::Unavailable("model.complete needs a session".into())
                })?;
                let input = input
                    .ok_or_else(|| ModError::Hook("model.complete input is required".into()))?;
                session.model_complete(input, plugin).await?
            }
            "ui.invalidate" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("ui.invalidate needs a session".into()))?;
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("ui.invalidate input is required".into()))?;
                match input.string_at("/event") {
                    Some("prompt.section") => session.invalidate_prompt_section().await?,
                    Some("prompt.context") => session.invalidate_prompt_context().await?,
                    Some("prompt.attachment") => session.invalidate_prompt_attachment().await?,
                    Some("tool.describe") => session.invalidate_tool_describe().await?,
                    Some("command.describe") => session.invalidate_command_describe().await?,
                    _ => {
                        return Err(ModError::Unavailable(
                            "ui.invalidate currently supports prompt.section, prompt.context, prompt.attachment, tool.describe, and command.describe".into(),
                        ));
                    }
                }
                static_projection("null")
            }
            "env.set" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("env.set input is required".into()))?;
                let name = env_name(event, input)?;
                let value = env_set_value(input)?;
                self.environment
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(name, value);
                static_projection("null")
            }
            "tool.check" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("tool.check input is required".into()))?;
                let tool = exact_string(input, "/tool")
                    .ok_or_else(|| ModError::Hook("tool.check tool must be a string".into()))?;
                if input.at_pointer("/tool_use_id").is_some() {
                    return Err(ModError::Hook(
                        "tool.check query cannot set tool_use_id".into(),
                    ));
                }
                let tool_input = input
                    .subprojection("/input")
                    .map_err(|_| ModError::Hook("tool.check input is required".into()))?;
                let session = session
                    .ok_or_else(|| ModError::Unavailable("tool.check needs a session".into()))?;
                session.tool_check(&tool, tool_input).await?
            }
            "tool.call" => {
                let input =
                    input.ok_or_else(|| ModError::Hook("tool.call input is required".into()))?;
                if input.string_at("/tool").is_none_or(str::is_empty) {
                    return Err(ModError::Hook(format!(
                        "{plugin}: $.tool.call takes the event's input: {{ tool, ...args }}"
                    )));
                }
                let session = session.ok_or_else(|| {
                    ModError::Unavailable(format!(
                        "{plugin}: $.tool.call needs a permission-aware session"
                    ))
                })?;
                let context = tool_call_context.ok_or_else(|| {
                    ModError::Protocol("tool.call is missing its host execution context".into())
                })?;
                session.tool_call(plugin, input, context).await?
            }
            "tool.list" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("tool.list needs a session".into()))?;
                session.tool_list().await?
            }
            "command.list" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("command.list needs a session".into()))?;
                session.command_list().await?
            }
            "command.register" => {
                let session = session.ok_or_else(|| {
                    ModError::Unavailable("command.register needs a session".into())
                })?;
                let input = input
                    .ok_or_else(|| ModError::Hook("command.register input is required".into()))?;
                session.command_register(plugin, input).await?
            }
            "command.run" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("command.run input is required".into()))?;
                let command_units = input.string_units("/command").ok_or_else(|| {
                    ModError::Hook(format!(
                        "{plugin}: $.command.run takes {{ command, args? }} (the command's name without the slash)"
                    ))
                })?;
                if command_units.is_empty() {
                    return Err(ModError::Hook(format!(
                        "{plugin}: $.command.run takes {{ command, args? }} (the command's name without the slash)"
                    )));
                }
                let command = input
                    .subprojection("/command")
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                let args = match input.at_pointer("/args") {
                    Some(_) if input.string_units("/args").is_some() => input
                        .subprojection("/args")
                        .map_err(|error| ModError::Protocol(error.to_string()))?,
                    None => root_string_projection(""),
                    Some(_) => {
                        return Err(ModError::Hook(format!(
                            "{plugin}: $.command.run takes {{ command, args? }} (the command's name without the slash)"
                        )));
                    }
                };
                let session = session
                    .ok_or_else(|| ModError::Unavailable("command.run needs a session".into()))?;
                session.command_run(plugin, command, args).await?
            }
            "prompt.submit" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("prompt.submit input is required".into()))?;
                let text_units = input.string_units("/text").ok_or_else(|| {
                    ModError::Hook(format!(
                        "{plugin}: $.prompt.submit takes {{ text }} (a non-empty prompt)"
                    ))
                })?;
                let trimmed = js_trimmed_units(&text_units);
                if trimmed.is_empty() {
                    return Err(ModError::Hook(format!(
                        "{plugin}: $.prompt.submit takes {{ text }} (a non-empty prompt)"
                    )));
                }
                if trimmed.first() == Some(&(b'/' as u16)) {
                    return Err(ModError::Hook(format!(
                        "{plugin}: $.prompt.submit submits a prompt to the model; a text beginning with / would run a command as the user; run one with $.command.run({{ command }})"
                    )));
                }
                let text = input
                    .subprojection("/text")
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                let as_user = input
                    .at_pointer("/asUser")
                    .and_then(JsonValueView::as_bool)
                    .unwrap_or(false);
                let session = session
                    .ok_or_else(|| ModError::Unavailable("prompt.submit needs a session".into()))?;
                session.prompt_submit(plugin, text, as_user).await?
            }
            "session.receive" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("session.receive input is required".into()))?;
                let text = input
                    .subprojection("/text")
                    .map_err(|_| ModError::Hook("session.receive text must be a string".into()))?;
                if input.string_units("/text").is_none() {
                    return Err(ModError::Hook(
                        "session.receive text must be a string".into(),
                    ));
                }
                let kind = exact_string(input, "/origin/kind").ok_or_else(|| {
                    ModError::Hook("session.receive origin.kind must be a string".into())
                })?;
                if !matches!(
                    kind.as_str(),
                    "bridge"
                        | "task-notification"
                        | "scheduled-trigger"
                        | "peer"
                        | "peer-send-message"
                        | "projects-relay"
                        | "slack-ping"
                        | "unclassified"
                ) {
                    return Err(ModError::Hook(
                        "session.receive origin.kind is unsupported".into(),
                    ));
                }
                projection_object(vec![("text".into(), text)])?
            }
            "tool.register" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("tool.register needs a session".into()))?;
                let input = input
                    .ok_or_else(|| ModError::Hook("tool.register input is required".into()))?;
                session.tool_register(plugin, input).await?
            }
            "session.cwd" => root_string_projection(&cwd.to_string_lossy()),
            "session.root" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("session.root needs a session".into()))?;
                let root = if cwd_is_pinned {
                    cwd.to_path_buf()
                } else {
                    session.root()
                };
                root_string_projection(&root.to_string_lossy())
            }
            "session.model" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("session.model needs a session".into()))?;
                root_string_projection(&session.model().await)
            }
            "session.id" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("session.id needs a session".into()))?;
                root_string_projection(&session.id().await)
            }
            "session.turns" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("session.turns needs a session".into()))?;
                projection_u64_value(session.turns().await)?
            }
            "session.repo" => generated_projection(&session_repo(&cwd).await)?,
            "session.version" => {
                let session = session.ok_or_else(|| {
                    ModError::Unavailable("session.version needs a session".into())
                })?;
                session.version().await?
            }
            "session.surfaces" => {
                let surfaces = session.map_or_else(Vec::new, |session| session.surfaces());
                Utf16JsonProjection::array(
                    surfaces
                        .iter()
                        .map(|surface| root_string_projection(surface))
                        .collect(),
                )
                .map_err(|error| ModError::Protocol(error.to_string()))?
            }
            "session.surface" => {
                match session.and_then(|session| session.surfaces().into_iter().next()) {
                    Some(surface) => root_string_projection(&surface),
                    None => static_projection("null"),
                }
            }
            "settings.read" => {
                let session = session.ok_or_else(|| {
                    ModError::Unavailable("settings.read needs a settings-aware session".into())
                })?;
                let input = input
                    .ok_or_else(|| ModError::Hook("settings.read input is required".into()))?;
                session.settings_read(input).await?
            }
            "clock.now" => {
                let elapsed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                projection_u64_value(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))?
            }
            "clock.sleep" | "clock.after" | "clock.every" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook(format!("{event} input is required")))?;
                let ms = input
                    .number_at("/ms")
                    .filter(|ms| ms.is_finite() && *ms >= 0.0)
                    .ok_or_else(|| {
                        ModError::Hook(format!("{event} ms must be a non-negative number"))
                    })?;
                let wait = Duration::try_from_secs_f64(ms / 1000.0).map_err(|_| {
                    ModError::Hook(format!("{event} ms is too large for this host"))
                })?;
                tokio::time::sleep(wait).await;
                static_projection("null")
            }
            "process.run" => {
                let input =
                    input.ok_or_else(|| ModError::Hook("process.run input is required".into()))?;
                let environment = self
                    .environment
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                run_process_api(input, &cwd, plugin, &environment).await?
            }
            "fs.read" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("fs.read input is required".into()))?;
                let raw = lossy_string(input, "/path")
                    .ok_or_else(|| ModError::Hook("fs.read path must be a string".into()))?;
                let path = resolve_mod_api_path(&raw, &cwd)?;
                let format = input.string_at("/as").unwrap_or("text");
                if format != "text" && format != "bytes" {
                    return Err(ModError::Hook("fs.read as must be text or bytes".into()));
                }
                let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
                    let file = std::fs::File::open(path)?;
                    let mut bytes = Vec::new();
                    file.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                    Ok(bytes)
                })
                .await
                .map_err(|error| ModError::Unavailable(error.to_string()))??;
                if bytes.len() > 4 * 1024 * 1024 {
                    return Err(ModError::Hook("fs.read exceeds 4 MiB".into()));
                }
                if format == "bytes" {
                    generated_projection(&json!({
                        "base64":base64::engine::general_purpose::STANDARD.encode(bytes)
                    }))?
                } else {
                    let text = String::from_utf8_lossy(&bytes);
                    root_string_projection(&text)
                }
            }
            "fs.write" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("fs.write input is required".into()))?;
                let raw = lossy_string(input, "/path")
                    .ok_or_else(|| ModError::Hook("fs.write path must be a string".into()))?;
                let text = lossy_string(input, "/text")
                    .ok_or_else(|| ModError::Hook("fs.write text must be a string".into()))?;
                if text.len() > 4 * 1024 * 1024 {
                    return Err(ModError::Hook("fs.write exceeds 4 MiB".into()));
                }
                let path = resolve_mod_api_path(&raw, &cwd)?;
                let bytes = text.into_bytes();
                tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(path, bytes)
                })
                .await
                .map_err(|error| ModError::Unavailable(error.to_string()))??;
                static_projection("null")
            }
            "fs.exists" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("fs.exists input is required".into()))?;
                let raw = lossy_string(input, "/path")
                    .ok_or_else(|| ModError::Hook("fs.exists path must be a string".into()))?;
                let path = resolve_mod_api_path(&raw, &cwd)?;
                let exists = tokio::task::spawn_blocking(move || path.exists())
                    .await
                    .map_err(|error| ModError::Unavailable(error.to_string()))?;
                generated_projection(&json!(exists))?
            }
            "fs.list" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("fs.list input is required".into()))?;
                let raw = lossy_string(input, "/path")
                    .ok_or_else(|| ModError::Hook("fs.list path must be a string".into()))?;
                let path = resolve_mod_api_path(&raw, &cwd)?;
                let entries = tokio::task::spawn_blocking(move || -> std::io::Result<Value> {
                    let mut entries = Vec::new();
                    for entry in std::fs::read_dir(path)? {
                        let entry = entry?;
                        let metadata = std::fs::symlink_metadata(entry.path())?;
                        let kind = if metadata.file_type().is_symlink() {
                            "other"
                        } else if metadata.is_file() {
                            "file"
                        } else if metadata.is_dir() {
                            "dir"
                        } else {
                            "other"
                        };
                        entries.push(json!({
                            "name": entry.file_name().to_string_lossy(),
                            "kind": kind,
                            "size": metadata.len(),
                            "isLink": metadata.file_type().is_symlink(),
                        }));
                    }
                    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                    Ok(Value::Array(entries))
                })
                .await
                .map_err(|error| ModError::Unavailable(error.to_string()))?
                .map_err(ModError::Io)?;
                generated_projection(&entries)?
            }
            "fs.stat" => {
                let input = input
                    .as_ref()
                    .ok_or_else(|| ModError::Hook("fs.stat input is required".into()))?;
                let raw = lossy_string(input, "/path")
                    .ok_or_else(|| ModError::Hook("fs.stat path must be a string".into()))?;
                let path = resolve_mod_api_path(&raw, &cwd)?;
                let resolve = input
                    .at_pointer("/resolve")
                    .and_then(JsonValueView::as_bool)
                    .unwrap_or(false);
                let result = tokio::task::spawn_blocking(move || -> std::io::Result<Value> {
                    let link = std::fs::symlink_metadata(&path)?;
                    let target = match std::fs::metadata(&path) {
                        Ok(metadata) => Some(metadata),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(error),
                    };
                    let metadata = target.as_ref().unwrap_or(&link);
                    let kind = if metadata.is_file() {
                        "file"
                    } else if metadata.is_dir() {
                        "dir"
                    } else {
                        "other"
                    };
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
                    let mtime_ms = modified.map_or(0.0, |value| value.as_secs_f64() * 1000.0);
                    let mut result = json!({
                        "kind": kind,
                        "size": metadata.len(),
                        "mtimeMs": mtime_ms,
                        "isLink": link.file_type().is_symlink(),
                    });
                    if resolve && target.is_some() {
                        if let Ok(real) = std::fs::canonicalize(&path) {
                            result["realPath"] = Value::String(real.to_string_lossy().into_owned());
                        }
                    }
                    Ok(result)
                })
                .await
                .map_err(|error| ModError::Unavailable(error.to_string()))?
                .map_err(ModError::Io)?;
                generated_projection(&result)?
            }
            "fs.ancestors" => {
                let session = session
                    .ok_or_else(|| ModError::Unavailable("fs.ancestors needs a session".into()))?;
                let input =
                    input.ok_or_else(|| ModError::Hook("fs.ancestors input is required".into()))?;
                session.fs_ancestors_at(input, &cwd).await?
            }
            _ => {
                return Err(ModError::Protocol(format!(
                    "unsupported host API event: {event}"
                )));
            }
        };
        result
            .validate()
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        Ok(HostApiValue::JsonProjection(result))
    }

    /// Evaluate a hooks module and call its exported `register(on, options)`.
    pub async fn load(
        &self,
        plugin: &str,
        root: &Path,
        module: &Path,
        options: Utf16JsonProjection,
    ) -> Result<(), ModError> {
        self.load_with_tier(plugin, root, module, options, "user")
            .await
    }

    /// Evaluate a module at its policy tier.
    pub async fn load_with_tier(
        &self,
        plugin: &str,
        root: &Path,
        module: &Path,
        options: Utf16JsonProjection,
        tier: &str,
    ) -> Result<(), ModError> {
        self.load_with_tier_order(plugin, root, module, options, tier, None)
            .await
    }

    /// Evaluate a module with its explicit order inside a managed tier.
    pub async fn load_with_tier_order(
        &self,
        plugin: &str,
        root: &Path,
        module: &Path,
        options: Utf16JsonProjection,
        tier: &str,
        tier_order: Option<u32>,
    ) -> Result<(), ModError> {
        self.load_with_tier_order_storage(plugin, plugin, root, module, options, tier, tier_order)
            .await
    }

    /// Keep installed-plugin storage identity separate from the displayed name.
    pub async fn load_with_tier_order_storage(
        &self,
        plugin: &str,
        storage_id: &str,
        root: &Path,
        module: &Path,
        options: Utf16JsonProjection,
        tier: &str,
        tier_order: Option<u32>,
    ) -> Result<(), ModError> {
        self.load_with_tier_order_storage_prepared(
            plugin, storage_id, root, module, options, tier, tier_order, None,
        )
        .await
    }

    /// Evaluate a previously scanned module after `plugin.register` allows it.
    #[allow(clippy::too_many_arguments)]
    pub async fn load_with_tier_order_storage_prepared(
        &self,
        plugin: &str,
        storage_id: &str,
        root: &Path,
        module: &Path,
        options: Utf16JsonProjection,
        tier: &str,
        tier_order: Option<u32>,
        prepared: Option<&ModPreparedModule>,
    ) -> Result<(), ModError> {
        if storage_id == SEC_DEFAULT_STORAGE_ID {
            return Err(ModError::Unavailable(
                "cc-plugin-sec-default@builtin is reserved for the host security default".into(),
            ));
        }
        let owned_prepared = if prepared.is_none() {
            Some(self.prepare_module(root, module).await?)
        } else {
            None
        };
        let prepared = prepared.or(owned_prepared.as_ref());
        // A reload must not inherit the old module's grants while the worker
        // evaluates the replacement and its new source scan is in flight.
        self.api_origin_contexts.unregister_storage(storage_id);
        let mut revoke_api_callers =
            RevokeApiCallersOnDrop::new(self.api_origin_contexts.clone(), storage_id);
        self.env_scans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(storage_id);
        let prepared_client_modules = prepared.and_then(|module| module.client_modules.clone());
        let mut request_message = Utf16JsonProjection::from_serde_value(&json!({
            "kind":"load",
            "plugin":plugin,
            "storageId":storage_id,
            "root":root,
            "module":module,
            "tier":tier,
            "tierOrder":tier_order,
            "preparedToken":prepared.map(|prepared| prepared.token.as_str())
        }))
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        request_message
            .set_field("options", options)
            .map_err(|error| ModError::Protocol(error.to_string()))?;
        let mut request = self.request_projection(request_message).await?;
        let reply = loop {
            let reply = self.receive(&mut request).await?;
            match reply.string_at("/kind") {
                Some("api.callers") => {
                    let call_id = projection_u64(&reply, "/callId").ok_or_else(|| {
                        ModError::Protocol("Mod API caller handshake missing call id".into())
                    })?;
                    let registrations = match parse_mod_api_callers(&reply, None) {
                        Ok(registrations) => registrations,
                        Err(error) => {
                            let _ = self.send_generated(&json!({"id":request.id,"kind":"api.callers.error","callId":call_id,"message":error.to_string()})).await;
                            self.api_origin_contexts.unregister_storage(storage_id);
                            return Err(error);
                        }
                    };
                    if let Err(error) =
                        self.api_origin_contexts
                            .register_storage(plugin, storage_id, registrations)
                    {
                        let error = ModError::Protocol(error.to_string());
                        let _ = self.send_generated(&json!({"id":request.id,"kind":"api.callers.error","callId":call_id,"message":error.to_string()})).await;
                        self.api_origin_contexts.unregister_storage(storage_id);
                        return Err(error);
                    }
                    self.send_generated(
                        &json!({"id":request.id,"kind":"api.callers.result","callId":call_id}),
                    )
                    .await?;
                }
                Some("loaded" | "error") => break reply,
                _ => {
                    return Err(ModError::Protocol(format!(
                        "unexpected load reply: {}",
                        reply.to_json_string().unwrap_or_default()
                    )));
                }
            }
        };
        match reply.string_at("/kind") {
            Some("loaded") => {
                let hook_count = reply.array_len("/hooks").ok_or_else(|| {
                    ModError::Protocol("Mod load reply missing hook events".into())
                })?;
                let hooks = (0..hook_count)
                    .map(|index| {
                        projection_string(&reply, &format!("/hooks/{index}"))
                            .filter(|event| !event.is_empty())
                            .ok_or_else(|| {
                                ModError::Protocol("Mod load reply has invalid hook event".into())
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let api_callers = parse_mod_api_callers(&reply, Some(&hooks))?;
                self.api_origin_contexts
                    .register_storage(plugin, storage_id, api_callers)
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                let sources = reply
                    .array_len("/sources")
                    .map(|count| {
                        (0..count)
                            .filter_map(|index| {
                                projection_string(&reply, &format!("/sources/{index}"))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let scan = if sources.len() <= 256
                    && sources.iter().map(String::len).sum::<usize>() <= 8 * 1024 * 1024
                {
                    tokio::task::spawn_blocking(move || scan::scan_sources(&sources))
                        .await
                        .unwrap_or_default()
                } else {
                    scan::EnvScan::default()
                };
                self.env_scans
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(storage_id.to_owned(), scan);
                self.loaded_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(storage_id.to_owned(), hooks);
                self.client_ui
                    .install_loaded_plugin(plugin, storage_id, prepared_client_modules);
                revoke_api_callers.disarm();
                self.registration_revision.fetch_add(1, Ordering::AcqRel);
                self.clear_all_mod_ui_press_targets();
                // Native invalidates the global render-version input after a
                // successful Hooks-set commit or explicit module reload. The
                // Host load reply is its single commit boundary.
                self.note_client_ui_global_render_change().await;
                Ok(())
            }
            Some("error") => {
                self.api_origin_contexts.unregister_storage(storage_id);
                Err(ModError::Hook(
                    reply
                        .string_at("/message")
                        .unwrap_or("unknown error")
                        .into(),
                ))
            }
            _ => Err(ModError::Protocol(format!(
                "unexpected load reply: {}",
                reply.to_json_string().unwrap_or_default()
            ))),
        }
    }

    /// Drop all registrations owned by one plugin before disable or reload.
    pub async fn unload(&self, plugin: &str) -> Result<(), ModError> {
        let plugin_name = self.client_ui.plugin_for_storage(plugin);
        let storage_was_loaded = self
            .loaded_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(plugin);
        if plugin != SEC_DEFAULT_STORAGE_ID {
            self.api_origin_contexts.unregister_storage(plugin);
        }
        // Native FU removes the module from the active set and invalidates
        // global ui.render consumers before it awaits the remaining table
        // resolution. This is the Host's matching removal-start boundary.
        if storage_was_loaded {
            self.note_client_ui_global_render_change().await;
        }
        let mut request = self
            .request_projection(generated_projection(
                &json!({"kind":"unload","storageId":plugin,"plugin":plugin_name}),
            )?)
            .await?;
        let reply = self.receive(&mut request).await?;
        match reply.string_at("/kind") {
            Some("unloaded") => {
                self.env_scans
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(plugin);
                self.loaded_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(plugin);
                let invalidated_sites = self.client_ui.unload_storage(plugin);
                if let Some(session) = self.background_session() {
                    session.tool_unregister_plugin(plugin);
                    session.command_unregister_plugin(plugin).await;
                }
                self.registration_revision.fetch_add(1, Ordering::AcqRel);
                self.clear_all_mod_ui_press_targets();
                if storage_was_loaded {
                    // FU performs a second global bump after Kae's asynchronous
                    // table-resolution boundary. The Host worker's `unloaded`
                    // response plus local table cleanup is its corresponding
                    // completion point.
                    self.note_client_ui_global_render_change().await;
                } else if !invalidated_sites.is_empty() {
                    if let Some(session) = self.background_session() {
                        self.invalidate_unloaded_client_sites(&invalidated_sites, session.as_ref())
                            .await;
                    }
                }
                Ok(())
            }
            Some("error") => Err(ModError::Hook(
                reply
                    .string_at("/message")
                    .unwrap_or("unknown error")
                    .into(),
            )),
            _ => Err(ModError::Protocol(format!(
                "unexpected unload reply: {}",
                reply.to_json_string().unwrap_or_default()
            ))),
        }
    }

    /// Run all matching handlers in registration order. `core` is invoked by
    /// each `next(e)` that reaches the end of the middleware chain.
    pub async fn dispatch<F, Fut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        core: F,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
    {
        self.dispatch_with_log(event, input, core, |_, _| async {})
            .await
    }

    /// Dispatch while forwarding `$.ui.log` to the host's visible output.
    pub async fn dispatch_with_log<F, Fut, L, LFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        core: F,
        on_log: L,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
    {
        let cwd = std::env::current_dir()?;
        self.dispatch_with_log_at(event, input, &cwd, core, on_log)
            .await
    }

    /// Like [`Self::dispatch_with_log`], with the live session cwd for Mod API
    /// filesystem calls. Hosts with per-session working directories use this.
    pub async fn dispatch_with_log_at<F, Fut, L, LFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        cwd: &Path,
        core: F,
        on_log: L,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
    {
        self.dispatch_with_ui_at_context(
            event,
            input,
            cwd,
            None,
            None,
            None,
            None,
            None,
            core,
            on_log,
            |_, _, _| async {},
            |_, _| async {},
        )
        .await
        .map(|outcome| outcome.result_projection)
    }

    /// Dispatch with a live host session for `$.session.*` and file operations.
    pub async fn dispatch_with_log_at_session<F, Fut, L, LFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        core: F,
        on_log: L,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
    {
        let cwd = session.cwd();
        self.dispatch_with_ui_at_context(
            event,
            input,
            &cwd,
            Some(session),
            None,
            None,
            None,
            None,
            core,
            on_log,
            |_, _, _| async {},
            |_, _| async {},
        )
        .await
        .map(|outcome| outcome.result_projection)
    }

    /// Dispatch with live UI callbacks for Mod logs, toasts, and pinned statuses.
    pub async fn dispatch_with_ui_at_session<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        let cwd = session.cwd();
        self.dispatch_with_ui_at_context(
            event,
            input,
            &cwd,
            Some(session),
            None,
            None,
            None,
            None,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
        .map(|outcome| outcome.result_projection)
    }

    /// Dispatch one parent UI render while correlating `state.get` reads with
    /// the Host-owned render request. The revision is only a transient read
    /// scope; it is never exposed as the Client failure state token.
    pub(super) async fn dispatch_ui_render_with_state_scope_at_session<
        F,
        Fut,
        L,
        LFut,
        T,
        TFut,
        S,
        SFut,
    >(
        &self,
        input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        host_render_revision: u64,
        session: &dyn ModSessionContext,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        if host_render_revision == 0 {
            return Err(ModError::Hook(
                "ui.render state scope needs a positive Host revision".into(),
            ));
        }
        let surface = input.string_at("/surface").unwrap_or_default().to_owned();
        let component = input.string_at("/component").unwrap_or_default().to_owned();
        let request_id = input.string_units("/requestId").unwrap_or_default();
        let site = ModUiRenderSiteKey {
            surface: surface.clone(),
            component: component.clone(),
            request_id: request_id.clone(),
        };
        let on_screen = ui_render_input_is_on_screen(&input);
        let pace = ModUiRenderPace::for_site(&surface, &component, on_screen);
        let had_previous_render = if surface == "terminal" {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ui_render_sites
                .contains_key(&site)
        } else {
            false
        };
        let cwd = session.cwd();
        let outcome = self
            .dispatch_with_utf16_at_context_scope(
                "ui.render",
                input,
                &cwd,
                Some(session),
                ModDispatchScope {
                    plugin_scope: None,
                    press_token: None,
                    ui_render_host_revision: Some(host_render_revision),
                },
                lingxi_core::host::task_registry::FieldPresence::Missing,
                core,
                on_log,
                on_toast,
                on_status,
            )
            .await;
        if surface == "terminal" {
            match outcome {
                Ok(outcome) => {
                    let update = self
                        .finish_client_ui_render_state(
                            &site.surface,
                            &site.component,
                            &site.request_id,
                            host_render_revision,
                            outcome.ui_render_plugins.clone(),
                            false,
                            pace,
                        )
                        .1;
                    self.apply_ui_invalidation_update(update, Some(session))
                        .await;
                    if pace == ModUiRenderPace::Live
                        && had_previous_render
                        && outcome
                            .ui_render_duration_ms
                            .is_some_and(|duration| duration > 34)
                    {
                        self.note_ui_render_slow_plugins(&outcome.ui_render_plugins);
                    }
                    Ok(outcome)
                }
                Err(error) => {
                    self.discard_client_ui_render_state(
                        &site.surface,
                        &site.component,
                        &site.request_id,
                        host_render_revision,
                    );
                    Err(error)
                }
            }
        } else {
            outcome
        }
    }

    /// Dispatch a Button press only when its full worker/action identity still
    /// belongs to the current render of this session's terminal AbovePrompt.
    pub async fn dispatch_ui_press_at_session<L, LFut, T, TFut, S, SFut>(
        &self,
        input: Utf16JsonProjection,
        site_revision: u64,
        session: &dyn ModSessionContext,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        let request = ui::parse_above_prompt_press_request(&input).map_err(ModError::Hook)?;
        if site_revision == 0 {
            return Err(ModError::Hook(
                "ui.press needs a positive site revision".into(),
            ));
        }
        let site = ModUiPressSite {
            session_id: session.id().await,
            surface: request.surface.clone(),
            component: request.component.clone(),
            request_id: request.request_id.clone(),
        };
        let target = ModUiPressTarget {
            site: site.clone(),
            plugin: request.button.plugin.clone(),
            element: request.button.element.clone(),
            handle: request.button.handle,
            worker_epoch: request.button.worker_epoch.clone(),
            render_revision: request.button.render_revision,
            site_revision,
        };
        {
            let registry = self
                .ui_press_registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if registry.revisions.get(&site).copied() != Some(site_revision)
                || !registry.targets.contains(&target)
            {
                return Ok(static_projection(r#"{"handled":false}"#));
            }
        }
        let public_event = Utf16JsonProjection::object_units(vec![
            (
                "plugin".encode_utf16().collect(),
                projection_from_string_units(&request.button.plugin)?,
            ),
            (
                "element".encode_utf16().collect(),
                projection_from_string_units(&request.button.element)?,
            ),
            (
                "component".encode_utf16().collect(),
                root_string_projection(&request.component),
            ),
            (
                "requestId".encode_utf16().collect(),
                projection_from_string_units(&request.request_id)?,
            ),
            (
                "surface".encode_utf16().collect(),
                root_string_projection(&request.surface),
            ),
        ])
        .map_err(|error| ModError::Protocol(error.to_string()))?;
        let private_token = generated_projection(&json!({
            "handle":request.button.handle,
            "workerEpoch":request.button.worker_epoch,
            "renderRevision":request.button.render_revision,
        }))?;
        let cwd = session.cwd();
        self.dispatch_with_ui_at_context(
            "ui.press",
            public_event,
            &cwd,
            Some(session),
            None,
            None,
            None,
            Some(private_token),
            |_| async { Ok(Some(static_projection(r#"{"handled":false}"#))) },
            on_log,
            on_toast,
            on_status,
        )
        .await
        .map(|outcome| outcome.result_projection)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_with_ui_at_session_origin<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        origin: Option<Value>,
        skip_hook_id: Option<u64>,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        let cwd = session.cwd();
        let api_origin = origin.as_ref().map_or(
            lingxi_core::host::task_registry::FieldPresence::Missing,
            |origin| lingxi_core::host::task_registry::FieldPresence::Value(origin.clone()),
        );
        self.dispatch_with_ui_at_context_and_api_origin(
            event,
            input,
            &cwd,
            Some(session),
            None,
            origin,
            skip_hook_id,
            None,
            None,
            None,
            api_origin,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
        .map(|outcome| outcome.result_projection)
    }

    /// Like [`Self::dispatch_with_ui_at_session`], retaining matched plugin
    /// names for the native `tool.check` decision-reason attribution.
    pub async fn dispatch_with_ui_meta_at_session<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        let cwd = session.cwd();
        self.dispatch_with_ui_at_context(
            event,
            input,
            &cwd,
            Some(session),
            None,
            None,
            None,
            None,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
    }

    /// Dispatch a child agent's tool call with its effective cwd while still
    /// reading the owning session's live model, id, policy, and UI callbacks.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_with_ui_at_session_cwd<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        agent_cwd: &Path,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        self.dispatch_with_ui_at_session_cwd_and_origin(
            event,
            input,
            session,
            agent_cwd,
            lingxi_core::host::task_registry::FieldPresence::Missing,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
    }

    /// Dispatch a child agent's call with both its effective cwd and a
    /// host-carried origin snapshot. The origin never travels through event
    /// input JSON; worker API closures receive only an opaque context ticket.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_with_ui_at_session_cwd_and_origin<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        session: &dyn ModSessionContext,
        agent_cwd: &Path,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<Utf16JsonProjection, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        let visible_origin = match &api_origin {
            lingxi_core::host::task_registry::FieldPresence::Value(value) => Some(value.clone()),
            lingxi_core::host::task_registry::FieldPresence::Missing
            | lingxi_core::host::task_registry::FieldPresence::Null => None,
        };
        self.dispatch_with_ui_at_context_and_api_origin(
            event,
            input,
            agent_cwd,
            Some(session),
            Some(agent_cwd),
            visible_origin,
            None,
            None,
            None,
            None,
            api_origin,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
        .map(|outcome| outcome.result_projection)
    }

    async fn dispatch_with_ui_at_context<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        cwd: &Path,
        session: Option<&dyn ModSessionContext>,
        cwd_override: Option<&Path>,
        origin: Option<Value>,
        skip_hook_id: Option<u64>,
        press_token: Option<Utf16JsonProjection>,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        self.dispatch_with_ui_at_context_and_api_origin(
            event,
            input,
            cwd,
            session,
            cwd_override,
            origin,
            skip_hook_id,
            press_token,
            None,
            None,
            lingxi_core::host::task_registry::FieldPresence::Missing,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
    }

    /// Dispatch an exact flat JSON projection with Host-owned plugin and UI scope.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_with_utf16_at_context_scope<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        cwd: &Path,
        session: Option<&dyn ModSessionContext>,
        scope: ModDispatchScope,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        mut core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        self.dispatch_with_ui_at_context_and_api_origin_with_utf16_projection(
            event,
            input,
            cwd,
            session,
            None,
            None,
            None,
            scope.press_token,
            scope.plugin_scope,
            scope.ui_render_host_revision,
            api_origin,
            move |input| core(input),
            on_log,
            on_toast,
            on_status,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_with_utf16_at_context<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        cwd: &Path,
        session: Option<&dyn ModSessionContext>,
        cwd_override: Option<&Path>,
        origin: Option<Value>,
        skip_hook_id: Option<u64>,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        self.dispatch_with_ui_at_context_and_api_origin_with_utf16_projection(
            event,
            input,
            cwd,
            session,
            cwd_override,
            origin,
            skip_hook_id,
            None,
            None,
            None,
            api_origin,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
    }

    async fn dispatch_with_ui_at_context_and_api_origin<F, Fut, L, LFut, T, TFut, S, SFut>(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        cwd: &Path,
        session: Option<&dyn ModSessionContext>,
        cwd_override: Option<&Path>,
        origin: Option<Value>,
        skip_hook_id: Option<u64>,
        press_token: Option<Utf16JsonProjection>,
        plugin_scope: Option<String>,
        ui_render_host_revision: Option<u64>,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        self.dispatch_with_ui_at_context_and_api_origin_with_utf16(
            event,
            input,
            cwd,
            session,
            cwd_override,
            origin,
            skip_hook_id,
            press_token,
            plugin_scope,
            ui_render_host_revision,
            api_origin,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
    }

    async fn dispatch_with_ui_at_context_and_api_origin_with_utf16<
        F,
        Fut,
        L,
        LFut,
        T,
        TFut,
        S,
        SFut,
    >(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        cwd: &Path,
        session: Option<&dyn ModSessionContext>,
        cwd_override: Option<&Path>,
        origin: Option<Value>,
        skip_hook_id: Option<u64>,
        press_token: Option<Utf16JsonProjection>,
        plugin_scope: Option<String>,
        ui_render_host_revision: Option<u64>,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        core: F,
        on_log: L,
        on_toast: T,
        on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        let mut core = core;
        self.dispatch_with_ui_at_context_and_api_origin_with_utf16_projection(
            event,
            input,
            cwd,
            session,
            cwd_override,
            origin,
            skip_hook_id,
            press_token,
            plugin_scope,
            ui_render_host_revision,
            api_origin,
            core,
            on_log,
            on_toast,
            on_status,
        )
        .await
    }

    async fn dispatch_with_ui_at_context_and_api_origin_with_utf16_projection<
        F,
        Fut,
        L,
        LFut,
        T,
        TFut,
        S,
        SFut,
    >(
        &self,
        event: &str,
        input: Utf16JsonProjection,
        cwd: &Path,
        session: Option<&dyn ModSessionContext>,
        cwd_override: Option<&Path>,
        origin: Option<Value>,
        skip_hook_id: Option<u64>,
        press_token: Option<Utf16JsonProjection>,
        plugin_scope: Option<String>,
        ui_render_host_revision: Option<u64>,
        api_origin: lingxi_core::host::task_registry::FieldPresence<Value>,
        mut core: F,
        mut on_log: L,
        mut on_toast: T,
        mut on_status: S,
    ) -> Result<ModDispatchOutcome, ModError>
    where
        F: FnMut(Option<Utf16JsonProjection>) -> Fut,
        Fut: Future<Output = Result<Option<Utf16JsonProjection>, ModError>>,
        L: FnMut(String, Utf16JsonProjection) -> LFut,
        LFut: Future<Output = ()>,
        T: FnMut(String, Utf16JsonProjection, u64) -> TFut,
        TFut: Future<Output = ()>,
        S: FnMut(String, Option<Utf16JsonProjection>) -> SFut,
        SFut: Future<Output = ()>,
    {
        let terminal_above_prompt_render =
            event == "ui.render" && input.string_at("/surface") == Some("terminal");
        if terminal_above_prompt_render {
            ui::validate_above_prompt_event(&input).map_err(ModError::Hook)?;
        } else if event == "ui.press" {
            ui::validate_ui_press_event(&input).map_err(ModError::Hook)?;
        }
        let live_cwd = cwd_override.map_or_else(
            || session.map_or_else(|| cwd.to_path_buf(), ModSessionContext::cwd),
            Path::to_path_buf,
        );
        let mut press_token = press_token;
        let press_href_admitted = press_token
            .as_ref()
            .and_then(|token| token.at_pointer("/hostPressHrefAdmitted"))
            .and_then(JsonValueView::as_bool)
            == Some(true);
        if press_href_admitted {
            if let Some(token) = press_token.as_mut() {
                token
                    .remove_field("hostPressHrefAdmitted")
                    .map_err(|error| ModError::Protocol(error.to_string()))?;
            }
        }
        let mut dispatch_message =
            generated_projection(&json!({"kind":"dispatch","event":event,"cwd":live_cwd,
            "origin":origin,"skipHookId":skip_hook_id,
            "secDefaultOrder":self.sec_default_order.load(Ordering::Acquire)}))?;
        set_projection_field(&mut dispatch_message, "input", input.clone())?;
        if press_href_admitted {
            set_projection_field(
                &mut dispatch_message,
                "pressHrefAdmitted",
                static_projection("true"),
            )?;
        }
        if let Some(plugin_scope) = plugin_scope {
            set_projection_field(
                &mut dispatch_message,
                "pluginScope",
                root_string_projection(&plugin_scope),
            )?;
        }
        if let Some(revision) = ui_render_host_revision {
            if event != "ui.render" || revision == 0 {
                return Err(ModError::Protocol(
                    "Host UI render scope is only valid for a positive ui.render revision".into(),
                ));
            }
            set_projection_field(
                &mut dispatch_message,
                "uiRenderHostRevision",
                projection_u64_value(revision)?,
            )?;
        }
        if let Some(press_token) = press_token {
            set_projection_field(&mut dispatch_message, "pressToken", press_token)?;
        }
        let generation_session = session.and_then(ModSessionContext::ui_invalidation_context);
        let mut request = self
            .request_projection_with_api_origin_and_session(
                dispatch_message,
                api_origin,
                generation_session,
                None,
            )
            .await?;
        type ClockCall<'a> = Pin<
            Box<dyn Future<Output = (u64, Option<Result<HostApiValue, ModError>>)> + Send + 'a>,
        >;
        let mut clock_calls: FuturesUnordered<ClockCall<'_>> = FuturesUnordered::new();
        let mut clock_abort = HashMap::<u64, ApiCallControl>::new();
        loop {
            let message = if clock_calls.is_empty() {
                self.receive(&mut request).await?
            } else {
                tokio::select! {
                    completed = clock_calls.next() => {
                        if let Some((call_id, result)) = completed {
                            let cancelled_without_result =
                                tool_call_cancelled_without_result(
                                    clock_abort.get(&call_id),
                                    &result,
                                );
                            clock_abort.remove(&call_id);
                            if cancelled_without_result {
                                self.send_generated(&json!({
                                    "id":request.id,
                                    "kind":"api.error",
                                    "callId":call_id,
                                    "message":TOOL_CALL_ABORTED_MESSAGE
                                })).await?;
                            } else {
                                match result {
                                    Some(Ok(value)) => {
                                        let reply = value.reply(request.id, call_id)?;
                                        self.send_projection(&reply).await?;
                                    }
                                    Some(Err(error)) => self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?,
                                    None => {}, // Non-tool API callers reject directly on AbortSignal.
                                }
                            }
                        }
                        continue;
                    }
                    reply = request.replies.recv() => reply.ok_or_else(|| ModError::Unavailable("worker exited".into()))?,
                }
            };
            match message.string_at("/kind") {
                Some("next") => {
                    let call_id = projection_u64(&message, "/callId")
                        .ok_or_else(|| ModError::Protocol("next request missing callId".into()))?;
                    let next_input = message
                        .at_pointer("/input")
                        .map(|_| message.subprojection("/input"))
                        .transpose()
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                    if event == "tool.check"
                        && !same_projection_field_opt(&Some(input.clone()), &next_input, "")
                    {
                        self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"tool.check identity is pinned"})).await?;
                        continue;
                    }
                    if event == "prompt.compose"
                        && !same_projection_field_opt(&Some(input.clone()), &next_input, "/model")
                    {
                        self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"prompt.compose model is pinned"})).await?;
                        continue;
                    }
                    if event == "prompt.submit"
                        && !next_input.as_ref().is_some_and(|forwarded| {
                            valid_prompt_submit_forwarded(&input, forwarded)
                        })
                    {
                        self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"prompt.submit origin and wait are pinned"})).await?;
                        continue;
                    }
                    if event == "session.compact"
                        && next_input.as_ref().is_none_or(|forwarded| {
                            !same_projection_field(&input, forwarded, "/trigger")
                                || !same_projection_field(&input, forwarded, "/agentId")
                                || forwarded
                                    .at_pointer("/instructions")
                                    .is_some_and(|value| value.code_units().is_none())
                                || forwarded
                                    .at_pointer("/messages")
                                    .and_then(JsonValueView::array_len)
                                    .is_none_or(|count| {
                                        (0..count).any(|index| {
                                            forwarded
                                                .at_pointer(&format!("/messages/{index}"))
                                                .and_then(JsonValueView::object_entries)
                                                .is_none()
                                        })
                                    })
                        })
                    {
                        self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":"session.compact trigger and agentId are pinned; instructions and messages must be valid"})).await?;
                        continue;
                    }
                    match core(next_input).await {
                        Ok(result) => {
                            let mut reply = generated_projection(&json!({
                                "id":request.id,
                                "kind":"next.result",
                                "callId":call_id,
                            }))?;
                            if let Some(result) = result {
                                result
                                    .validate()
                                    .map_err(|error| ModError::Protocol(error.to_string()))?;
                                set_projection_field(&mut reply, "result", result)?;
                            }
                            self.send_projection(&reply).await?;
                        }
                        Err(error) => {
                            self.send_generated(&json!({"id":request.id,"kind":"next.error","callId":call_id,"message":error.to_string()})).await?;
                        }
                    }
                }
                Some("api") => {
                    let call_id = projection_u64(&message, "/callId")
                        .ok_or_else(|| ModError::Protocol("api request missing callId".into()))?;
                    let api_input = message
                        .at_pointer("/input")
                        .map(|_| message.subprojection("/input"))
                        .transpose()
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                    let api_context = match self.resolve_api_context(&message, Some(&request)) {
                        Ok(context) => context,
                        Err(error) => {
                            self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?;
                            continue;
                        }
                    };
                    let origin = json!({
                        "plugin":api_context.caller.plugin,
                        "tier":message.string_at("/tier").unwrap_or("user")
                    });
                    let method = message.string_at("/method").unwrap_or_default().to_owned();
                    let ui_render_state_read = match parse_ui_render_state_read_scope(
                        &message,
                        &method,
                        event,
                        &input,
                        ui_render_host_revision,
                    ) {
                        Ok(scope) => scope,
                        Err(error) => {
                            self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?;
                            continue;
                        }
                    };
                    if matches!(
                        method.as_str(),
                        "clock.sleep"
                            | "process.run"
                            | "model.complete"
                            | "tool.call"
                            | "agent.spawn"
                    ) {
                        if method == "tool.call" && api_input.is_none() {
                            self.send_generated(&json!({
                                "id":request.id,
                                "kind":"api.error",
                                "callId":call_id,
                                "message":"tool.call requires input"
                            }))
                            .await?;
                            continue;
                        }
                        let (abort, registration) = AbortHandle::new_pair();
                        let cancellation = lingxi_core::host::CancellationToken::new();
                        let control =
                            ApiCallControl::new(abort, cancellation.clone(), method == "tool.call");
                        clock_abort.insert(call_id, control.clone());
                        let input_for_tool = api_input.clone();
                        let hook_id = projection_u64(&message, "/hookId");
                        let request_id = request.id;
                        let storage_id = message
                            .string_at("/storageId")
                            .unwrap_or("unknown")
                            .to_owned();
                        let generation_ticket = message
                            .string_at("/generationContextTicket")
                            .map(str::to_owned);
                        let api_context_for_call = api_context.clone();
                        clock_calls.push(Box::pin(async move {
                            let context_slot = Arc::new(StdMutex::new(None));
                            let context_for_call = context_slot.clone();
                            let execution = Abortable::new(
                                async {
                                    let tool_call_context = if method == "tool.call" {
                                        let input = input_for_tool
                                            .as_ref()
                                            .expect("tool.call input validated above");
                                        match acquire_tool_call_context(
                                            session,
                                            cancellation.clone(),
                                            input,
                                            &api_context_for_call,
                                        )
                                        .await
                                        {
                                            Ok(context) => Some(context),
                                            Err(error) => return (Err(error), None),
                                        }
                                    } else {
                                        None
                                    };
                                    *context_for_call
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        tool_call_context.clone();
                                    let _cancel_tool_call_on_drop =
                                        CancelToolCallOnDrop::new(tool_call_context.as_ref());
                                    let result = self
                                        .dispatch_host_api_with_projection(
                                            &method,
                                            api_input,
                                            cwd,
                                            session,
                                            cwd_override.is_some(),
                                            0,
                                            origin,
                                            api_context_for_call,
                                            storage_id,
                                            hook_id,
                                            request_id,
                                            tool_call_context.clone(),
                                            generation_ticket,
                                        )
                                        .await;
                                    (result, tool_call_context)
                                },
                                registration,
                            )
                            .await
                            .ok();
                            let completed = execution.is_some();
                            let result = execution.map(|(result, _context)| result);
                            let tool_call_context = context_slot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .clone();
                            if let (Some(context), Some(session)) =
                                (tool_call_context.as_ref(), session)
                            {
                                if result.as_ref().is_none_or(Result::is_err) {
                                    let _ = abort_tool_call(session, context).await;
                                }
                            }
                            if completed {
                                control.finish();
                            }
                            (call_id, result)
                        }));
                        continue;
                    }
                    let supported = matches!(
                        method.as_str(),
                        "ui.invalidate"
                            | "prompt.compose"
                            | "prompt.context"
                            | "ui.log"
                            | "ui.toast"
                            | "ui.notify"
                            | "ui.status"
                            | "ui.selection"
                            | "telemetry.log"
                            | "telemetry.mark"
                            | "tool.call"
                            | "tool.check"
                            | "tool.list"
                            | "agent.list"
                            | "agent.spawn"
                            | "command.list"
                            | "command.register"
                            | "command.run"
                            | "prompt.submit"
                            | "tool.register"
                            | "clock.now"
                            | "clock.sleep"
                            | "clock.after"
                            | "clock.every"
                            | "process.run"
                            | "env.get"
                            | "env.set"
                            | "session.cwd"
                            | "session.root"
                            | "session.model"
                            | "session.id"
                            | "session.turns"
                            | "session.repo"
                            | "session.version"
                            | "session.receive"
                            | "session.messages"
                            | "session.usage"
                            | "session.surfaces"
                            | "session.surface"
                            | "model.fork"
                            | "model.complete"
                            | "model.classify"
                            | "settings.read"
                            | "fs.read"
                            | "fs.write"
                            | "fs.exists"
                            | "fs.list"
                            | "fs.stat"
                            | "fs.ancestors"
                            | "store.get"
                            | "store.set"
                            | "store.delete"
                            | "store.keys"
                            | "state.get"
                            | "state.set"
                    );
                    let result = if supported {
                        self.dispatch_host_api_with_ui_render_state_read_projection(
                            &method,
                            api_input,
                            cwd,
                            session,
                            cwd_override.is_some(),
                            0,
                            origin,
                            api_context,
                            message
                                .string_at("/storageId")
                                .unwrap_or("unknown")
                                .to_owned(),
                            projection_u64(&message, "/hookId"),
                            request.id,
                            None,
                            ui_render_state_read,
                            message_generation_context_ticket(&message)?,
                        )
                        .await
                    } else {
                        Err(ModError::Protocol(format!(
                            "unsupported Mod API method: {method}"
                        )))
                    };
                    match result {
                        Ok(result) => {
                            let reply = result.reply(request.id, call_id)?;
                            self.send_projection(&reply).await?;
                        }
                        Err(error) => {
                            self.send_generated(&json!({"id":request.id,"kind":"api.error","callId":call_id,"message":error.to_string()})).await?;
                        }
                    }
                }
                Some("api.cancel") => {
                    if let Some(call_id) = projection_u64(&message, "/callId") {
                        if let Some(control) = clock_abort.get(&call_id) {
                            control.cancel();
                        }
                    }
                }
                Some("result") => {
                    request.completed = true;
                    let result = message
                        .subprojection("/result")
                        .map_err(|error| ModError::Protocol(error.to_string()))?;
                    if terminal_above_prompt_render && result.string_at("/type") != Some("engine") {
                        ui::above_prompt_lines(&result).map_err(ModError::Hook)?;
                    }
                    let hooked = message
                        .array_len("/hooked")
                        .map(|count| {
                            (0..count)
                                .filter_map(|index| {
                                    message
                                        .string_at(&format!("/hooked/{index}"))
                                        .map(str::to_owned)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let all_hooked_builtin = message
                        .at_pointer("/allHookedBuiltin")
                        .and_then(JsonValueView::as_bool)
                        .unwrap_or(false);
                    let ui_render_duration_ms = if event == "ui.render" {
                        Some(
                            projection_u64(&message, "/uiRenderDurationMs")
                                .filter(|duration| *duration <= 9_007_199_254_740_991)
                                .ok_or_else(|| {
                                    ModError::Protocol(
                                        "ui.render result missing a valid worker duration".into(),
                                    )
                                })?,
                        )
                    } else {
                        None
                    };
                    let ui_render_plugins = if event == "ui.render" {
                        let plugin_count =
                            message.array_len("/uiRenderPlugins").ok_or_else(|| {
                                ModError::Protocol(
                                    "ui.render result missing matched plugin list".into(),
                                )
                            })?;
                        let mut seen = HashSet::new();
                        (0..plugin_count)
                            .map(|index| {
                                message
                                    .string_at(&format!("/uiRenderPlugins/{index}"))
                                    .filter(|plugin| !plugin.is_empty())
                                    .map(str::to_owned)
                                    .ok_or_else(|| {
                                        ModError::Protocol(
                                            "ui.render result has invalid matched plugin".into(),
                                        )
                                    })
                            })
                            .collect::<Result<Vec<_>, _>>()?
                            .into_iter()
                            .filter(|plugin| seen.insert(plugin.clone()))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    return Ok(ModDispatchOutcome {
                        result_projection: result,
                        hooked,
                        all_hooked_builtin,
                        ui_render_plugins,
                        ui_render_duration_ms,
                    });
                }
                Some("progress") => {}
                Some("log") => {
                    let plugin = message.string_at("/plugin").unwrap_or("unknown").to_owned();
                    let line = message
                        .subprojection("/text")
                        .unwrap_or_else(|_| root_string_projection(""));
                    if message.string_at("/to") == Some("debug") {
                        tracing::debug!(
                            plugin,
                            text = line.string_at("").unwrap_or_default(),
                            "mod log"
                        );
                    } else {
                        tracing::info!(
                            plugin,
                            text = line.string_at("").unwrap_or_default(),
                            "mod log"
                        );
                        on_log(plugin, line).await;
                    }
                }
                Some("toast") => {
                    let plugin = message.string_at("/plugin").unwrap_or("unknown");
                    let text = message
                        .subprojection("/text")
                        .unwrap_or_else(|_| root_string_projection(""));
                    let timeout_ms = projection_u64(&message, "/timeoutMs").unwrap_or(4000);
                    on_toast(plugin.to_owned(), text, timeout_ms).await;
                }
                Some("notify") => {
                    let plugin = message.string_at("/plugin").unwrap_or("engine");
                    if let Some(text) = message.subprojection("/text").ok() {
                        if let Some(session) = session.as_ref() {
                            session.emit_mod_notify(plugin, text).await;
                        }
                    }
                }
                Some("status") => {
                    let plugin = message.string_at("/plugin").unwrap_or("unknown");
                    let text = message
                        .at_pointer("/text")
                        .filter(|value| !value.is_null())
                        .and_then(|_| message.subprojection("/text").ok());
                    on_status(plugin.to_owned(), text).await;
                }
                Some("log.error") => {
                    tracing::warn!(message = %message.to_json_string().unwrap_or_default(), "mod log call dropped");
                }
                Some("error") => {
                    return Err(ModError::Hook(
                        message
                            .string_at("/message")
                            .unwrap_or("unknown error")
                            .into(),
                    ));
                }
                Some("protocol.error") => {
                    return Err(ModError::Protocol(
                        message
                            .string_at("/message")
                            .unwrap_or("invalid Mod worker protocol reply")
                            .into(),
                    ));
                }
                _ => {
                    return Err(ModError::Protocol(format!(
                        "unexpected dispatch reply: {}",
                        message.to_json_string().unwrap_or_default()
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_mod_options(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).expect("fixture options are valid JSON")
    }

    fn test_projection(value: Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).expect("fixture value is valid JSON")
    }

    fn projected(value: Value) -> Utf16JsonProjection {
        test_projection(value)
    }

    fn mod_test_projection(source: &str) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(source).unwrap()
    }

    trait ModTestJsonValue {
        fn mod_test_json_value(&self) -> Value;
    }

    impl ModTestJsonValue for Utf16JsonProjection {
        fn mod_test_json_value(&self) -> Value {
            serde_json::from_str(&self.to_json_string().unwrap()).unwrap()
        }
    }

    impl ModTestJsonValue for Value {
        fn mod_test_json_value(&self) -> Value {
            self.clone()
        }
    }

    fn mod_test_value<T: ModTestJsonValue + ?Sized>(value: &T) -> Value {
        value.mod_test_json_value()
    }

    fn mod_test_event(event: Option<Utf16JsonProjection>) -> Value {
        event.as_ref().map(mod_test_value).unwrap_or_default()
    }

    #[tokio::test]
    async fn queued_epoch_write_is_rejected_if_the_worker_dies_while_waiting_for_stdin() {
        let host = ModHost::start(None).await.unwrap();
        let epoch = host.current_worker_epoch().await;
        let held_input = epoch.input.lock().await;
        let send_host = host.clone();
        let send_epoch = epoch.clone();
        let (started_tx, started_rx) = oneshot::channel();
        let send = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            send_host
                .send_serialized_in_epoch(&send_epoch, 0, br#"{"id":0,"kind":"probe"}"#)
                .await
        });
        started_rx.await.unwrap();
        report_worker_epoch_failure(
            &epoch,
            &host.worker_failure_sender,
            "test epoch died".into(),
            None,
        );
        drop(held_input);
        let result = tokio::time::timeout(Duration::from_secs(5), send)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(ModError::Unavailable(_))));
    }

    #[tokio::test]
    async fn worker_eof_fails_current_dispatch_once_then_replays_on_a_new_epoch() {
        let root = tempfile::tempdir().unwrap();
        let module_path = root.path().join("replay.js");
        std::fs::write(
            &module_path,
            r#"export function register(on) {
              on('tool.call', async ($, event, next) => {
                const result = await next(event);
                return { ...result, result: 'replayed' };
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        let mut failures = host.subscribe_worker_failures().unwrap();
        let prepared = host
            .prepare_module(root.path(), &module_path)
            .await
            .unwrap();
        let registration = host
            .dispatch_plugin_register(test_projection(json!({
                "name":"replay",
                "tier":"user",
                "root":root.path().to_string_lossy(),
                "uses":prepared.uses.clone(),
                "provenance":"local",
            })))
            .await
            .unwrap();
        assert_eq!(mod_test_value(&registration)["allow"], true);
        host.load_with_tier_order_storage_prepared(
            "replay",
            "replay@local",
            root.path(),
            &module_path,
            test_mod_options(json!({})),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();

        let core_calls = Arc::new(AtomicU64::new(0));
        let first_core_calls = core_calls.clone();
        let first = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                move |_| {
                    first_core_calls.fetch_add(1, Ordering::AcqRel);
                    async move { Ok(Some(test_projection(json!({"result":"core"})))) }
                },
            )
            .await
            .unwrap();
        let first = mod_test_value(&first);
        assert_eq!(first["result"], "replayed");
        let old_epoch = host.current_worker_epoch().await.id;

        host.terminate_current_worker_for_test().await;
        let failed_core_calls = core_calls.clone();
        assert!(host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                move |_| {
                    failed_core_calls.fetch_add(1, Ordering::AcqRel);
                    async move { Ok(Some(test_projection(json!({"result":"core"})))) }
                }
            )
            .await
            .is_err());
        assert_eq!(core_calls.load(Ordering::Acquire), 1);
        let failure = tokio::time::timeout(Duration::from_secs(2), failures.recv())
            .await
            .expect("EOF publishes one worker failure")
            .expect("failure stream remains open");
        assert_eq!(failure.epoch, old_epoch);
        assert!(failure.attributed_storage_id.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), failures.recv())
                .await
                .is_err(),
            "one worker epoch publishes only one failure"
        );

        let replay_failures = host
            .recover_epoch(&[ModReplayModule {
                plugin: "replay".into(),
                storage_id: "replay@local".into(),
                root: root.path().to_path_buf(),
                module: module_path.clone(),
                options: test_mod_options(json!({})),
                tier: "user".into(),
                tier_order: None,
                version: None,
                provenance: "local".into(),
            }])
            .await
            .unwrap();
        assert!(replay_failures.is_empty());
        assert!(host.current_worker_epoch().await.id > old_epoch);
        let final_core_calls = core_calls.clone();
        let after = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                move |_| {
                    final_core_calls.fetch_add(1, Ordering::AcqRel);
                    async move { Ok(Some(test_projection(json!({"result":"core"})))) }
                },
            )
            .await
            .unwrap();
        let after = mod_test_value(&after);
        assert_eq!(after["result"], "replayed");
        assert_eq!(core_calls.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn late_request_id_never_sends_on_the_replacement_epoch() {
        async fn echo_worker(label: &str) -> (Child, ChildStdin, ChildStdout) {
            let source = format!(
                "process.stdin.on('data', data => process.stdout.write({label:?} + ':' + data))"
            );
            let mut child = Command::new("node")
                .arg("-e")
                .arg(source)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let input = child.stdin.take().unwrap();
            let output = child.stdout.take().unwrap();
            (child, input, output)
        }

        let host = ModHost::start(None).await.unwrap();
        let (old_child, old_input, old_output) = echo_worker("old").await;
        let (new_child, new_input, new_output) = echo_worker("new").await;
        let (old_epoch, old_background) =
            ModWorkerEpoch::new(100, old_child, old_input, None, false);
        let (new_epoch, new_background) =
            ModWorkerEpoch::new(101, new_child, new_input, None, false);
        drop(old_background);
        drop(new_background);
        old_epoch.status.store(WORKER_EPOCH_DEAD, Ordering::Release);
        host.active_request_epochs
            .lock()
            .unwrap()
            .insert(900, Arc::downgrade(&old_epoch));
        *host.worker_epoch.write().await = new_epoch;

        assert!(host
            .send_generated(&json!({"id":900,"kind":"late.reply"}))
            .await
            .is_err());
        assert!(host
            .send_generated_in_epoch(&old_epoch, &json!({"id":0,"kind":"late.background.reply"}))
            .await
            .is_err());
        host.send_generated(&json!({"id":0,"kind":"current.probe"}))
            .await
            .unwrap();
        let mut new_lines = BufReader::new(new_output).lines();
        let line = tokio::time::timeout(Duration::from_secs(1), new_lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(line.starts_with("new:"));
        assert!(line.contains("current.probe"));
        let mut old_lines = BufReader::new(old_output).lines();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), old_lines.next_line())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn prompt_context_result_preserves_exact_strings_and_lone_keys() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("utf16-context-result.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('prompt.context', (api, event) => ({
                blocks: [{ name: `name\uD800`, text: `text\uD801`, [`key\uD802`]: `value\uD803` }],
                instructionFiles: [],
              }));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "utf16-context-result",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let input = Utf16JsonProjection::parse(r#"{"blocks":[],"instructionFiles":[]}"#).unwrap();
        let outcome = host
            .dispatch_with_utf16_at_context(
                "prompt.context",
                input,
                root.path(),
                None,
                None,
                None,
                None,
                lingxi_core::host::task_registry::FieldPresence::Missing,
                |projection| async move { Ok(projection) },
                |_, _| async {},
                |_, _, _| async {},
                |_, _| async {},
            )
            .await
            .unwrap();
        let units = |prefix: &str, unit: u16| {
            let mut units: Vec<_> = prefix.encode_utf16().collect();
            units.push(unit);
            units
        };
        assert_eq!(
            outcome.result_projection.string_units("/blocks/0/name"),
            Some(units("name", 0xd800))
        );
        assert_eq!(
            outcome.result_projection.string_units("/blocks/0/text"),
            Some(units("text", 0xd801))
        );
        let block = outcome.result_projection.at_pointer("/blocks/0").unwrap();
        let dynamic_key = units("key", 0xd802);
        let dynamic_value = block
            .object_entries()
            .unwrap()
            .into_iter()
            .find(|entry| entry.code_units == dynamic_key.as_slice())
            .unwrap()
            .value;
        assert_eq!(
            dynamic_value.code_units(),
            Some(units("value", 0xd803)).as_deref()
        );
    }

    #[test]
    fn worker_line_parser_keeps_exact_utf16_payload_and_property_units() {
        let source = static_projection(
            r#"{"id":73,"kind":"result","result":{"text":"\ud800","\ud801":"\ud802"}}"#,
        );
        let frame = encode_mod_ipc_frame(&source)
            .unwrap()
            .to_json_string()
            .unwrap();
        let routed = parse_mod_worker_line(&frame).unwrap();
        assert_eq!(projection_u64(&routed, "/id"), Some(73));
        assert_eq!(routed.string_at("/kind"), Some("result"));
        assert_eq!(routed.string_units("/result/text"), Some(vec![0xd800]));
        let entries = routed
            .at_pointer("/result")
            .and_then(|value| value.object_entries())
            .unwrap();
        assert_eq!(entries[1].code_units, &[0xd801]);
        assert_eq!(entries[1].value.code_units(), Some(&[0xd802][..]));

        let encoded = encode_mod_ipc_frame(&routed)
            .unwrap()
            .to_json_string()
            .unwrap();
        let reparsed = parse_mod_worker_line(&encoded).unwrap();
        assert_eq!(reparsed.string_units("/result/text"), Some(vec![0xd800]));
        let reparsed_entries = reparsed
            .at_pointer("/result")
            .and_then(|value| value.object_entries())
            .unwrap();
        assert_eq!(reparsed_entries[1].code_units, &[0xd801]);
        assert_eq!(reparsed_entries[1].value.code_units(), Some(&[0xd802][..]));
    }

    #[test]
    fn worker_ipc_frame_preserves_exceptional_numbers_and_reserved_looking_user_data() {
        let nan_bits = 0x7ff8_0000_0000_1234;
        let mut builder = Utf16JsonBuilder::new();
        let nan = builder.push_number(f64::from_bits(nan_bits));
        let positive_infinity = builder.push_number(f64::INFINITY);
        let negative_infinity = builder.push_number(f64::NEG_INFINITY);
        let negative_zero = builder.push_number(-0.0);
        let slash_tilde = builder.push_number(f64::INFINITY);
        let payload = builder
            .push_object_units(vec![
                ("nan".encode_utf16().collect(), nan),
                (
                    "positiveInfinity".encode_utf16().collect(),
                    positive_infinity,
                ),
                (
                    "negativeInfinity".encode_utf16().collect(),
                    negative_infinity,
                ),
                ("negativeZero".encode_utf16().collect(), negative_zero),
                ("/~".encode_utf16().collect(), slash_tilde),
            ])
            .unwrap();
        let marker_protocol = builder.push_string(MOD_IPC_PROTOCOL);
        let marker_version = builder.push_number(MOD_IPC_VERSION as f64);
        let marker_message = builder.push_null();
        let marker_bits = builder.push_array(vec![]).unwrap();
        let marker = builder
            .push_object(vec![
                ("protocol".into(), marker_protocol),
                ("version".into(), marker_version),
                ("message".into(), marker_message),
                ("numberBits".into(), marker_bits),
            ])
            .unwrap();
        let id = builder.push_number(73.0);
        let kind = builder.push_string("result");
        let message = builder
            .push_object(vec![
                ("id".into(), id),
                ("kind".into(), kind),
                ("payload".into(), payload),
                ("userMarker".into(), marker),
            ])
            .unwrap();
        let source = builder.finish(message).unwrap();
        let encoded = encode_mod_ipc_frame(&source)
            .unwrap()
            .to_json_string()
            .unwrap();
        let decoded = parse_mod_worker_line(&encoded).unwrap();

        assert_eq!(
            decoded.number_at("/payload/nan").unwrap().to_bits(),
            nan_bits
        );
        assert_eq!(
            decoded.number_at("/payload/positiveInfinity").unwrap(),
            f64::INFINITY
        );
        assert_eq!(
            decoded.number_at("/payload/negativeInfinity").unwrap(),
            f64::NEG_INFINITY
        );
        assert_eq!(
            decoded
                .number_at("/payload/negativeZero")
                .unwrap()
                .to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(decoded.number_at("/payload/~1~0"), Some(f64::INFINITY));
        assert_eq!(
            decoded.string_at("/userMarker/protocol"),
            Some(MOD_IPC_PROTOCOL)
        );
        assert_eq!(decoded.number_at("/userMarker/version"), Some(1.0));
        assert_eq!(decoded.array_len("/userMarker/numberBits"), Some(0));
    }

    #[tokio::test]
    async fn worker_dispatch_roundtrips_exceptional_numbers_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("ipc-exceptional-numbers.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', async (_$, event) => ({ result: event.values }));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "ipc-exceptional-numbers",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();

        let mut builder = Utf16JsonBuilder::new();
        let tool = builder.push_string("ExceptionalNumbers");
        let empty_input = builder.push_object(vec![]).unwrap();
        let nan = builder.push_number(f64::NAN);
        let positive_infinity = builder.push_number(f64::INFINITY);
        let negative_infinity = builder.push_number(f64::NEG_INFINITY);
        let negative_zero = builder.push_number(-0.0);
        let values = builder
            .push_object(vec![
                ("nan".into(), nan),
                ("positiveInfinity".into(), positive_infinity),
                ("negativeInfinity".into(), negative_infinity),
                ("negativeZero".into(), negative_zero),
            ])
            .unwrap();
        let event = builder
            .push_object(vec![
                ("tool".into(), tool),
                ("input".into(), empty_input),
                ("values".into(), values),
            ])
            .unwrap();
        let event = builder.finish(event).unwrap();

        let result = host
            .dispatch("tool.call", event, |_| async {
                panic!("Mod returns exceptional-number result")
            })
            .await
            .unwrap();
        assert!(result.number_at("/result/nan").unwrap().is_nan());
        assert_eq!(
            result.number_at("/result/positiveInfinity"),
            Some(f64::INFINITY)
        );
        assert_eq!(
            result.number_at("/result/negativeInfinity"),
            Some(f64::NEG_INFINITY)
        );
        assert_eq!(
            result.number_at("/result/negativeZero").unwrap().to_bits(),
            (-0.0_f64).to_bits()
        );
    }

    #[test]
    fn api_reply_writes_direct_json_and_keeps_undefined_distinct_from_null() {
        let exact =
            Utf16JsonProjection::root_string("value�".into(), vec![118, 97, 108, 117, 101, 0xd800])
                .unwrap();
        let reply = HostApiValue::JsonProjection(exact).reply(7, 9).unwrap();
        assert_eq!(
            reply.string_units("/result"),
            Some(vec![118, 97, 108, 117, 101, 0xd800])
        );
        let wire = reply.to_json_string().unwrap();
        assert!(wire.contains(r#""result":"value\ud800""#));
        assert!(!wire.contains("__lingxiModUtf16"));

        let null_reply = HostApiValue::JsonProjection(Utf16JsonProjection::parse("null").unwrap())
            .reply(7, 9)
            .unwrap();
        assert!(null_reply.get_property("result").unwrap().is_null());
        let undefined_reply = HostApiValue::Undefined.reply(7, 9).unwrap();
        assert!(undefined_reply.get_property("result").is_none());
    }

    #[tokio::test]
    async fn dispatch_round_trips_exact_source_projection() {
        let host = ModHost::start(None).await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let input = Utf16JsonProjection::parse(r#"{"answer":"\ud800","\ud801":"\ud802"}"#).unwrap();
        let outcome = host
            .dispatch_with_utf16_at_context(
                "turn.complete",
                input,
                root.path(),
                None,
                None,
                None,
                None,
                lingxi_core::host::task_registry::FieldPresence::Missing,
                |projection| async move { Ok(projection) },
                |_, _| async {},
                |_, _, _| async {},
                |_, _| async {},
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.result_projection.string_units("/answer"),
            Some(vec![0xd800])
        );
        let entries = outcome
            .result_projection
            .at_pointer("")
            .and_then(|value| value.object_entries())
            .unwrap();
        assert_eq!(entries[1].code_units, &[0xd801]);
        assert_eq!(entries[1].value.code_units(), Some(&[0xd802][..]));
    }

    #[tokio::test]
    async fn ui_press_preflight_result_completes_request_without_drop_cancel() {
        let host = ModHost::start(None).await.unwrap();
        let mut child = Command::new("node")
            .args(["-e", "process.stdin.pipe(process.stdout)"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let routes: Routes = Arc::new(StdMutex::new(HashMap::new()));
        let (sender, replies) = mpsc::unbounded_channel();
        let id = 73;
        routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, sender.clone());
        let ticket = "preflight-caller-ticket".to_owned();
        let mut channel = RequestChannel {
            id,
            api_context_ticket: Some(ticket.clone()),
            generation_context_ticket: None,
            generation_api_sessions: Arc::new(StdMutex::new(HashMap::new())),
            replies,
            routes,
            input: input.clone(),
            worker_epoch: None,
            active_request_epochs: None,
            completed: false,
        };
        drop(input);

        sender
            .send(
                Utf16JsonProjection::parse(
                    r#"{"kind":"ui.press.preflight.result","admitted":true}"#,
                )
                .unwrap(),
            )
            .unwrap();
        let reply = host.receive(&mut channel).await.unwrap();
        assert_eq!(reply.string_at("/kind"), Some("ui.press.preflight.result"));
        assert_eq!(
            reply
                .at_pointer("/admitted")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(channel.api_context_ticket.as_deref(), Some(ticket.as_str()));
        assert!(channel.completed);
        drop(sender);
        drop(channel);

        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        let first_line = tokio::time::timeout(Duration::from_millis(100), output.next_line()).await;
        assert!(
            matches!(&first_line, Ok(Ok(None)) | Err(_)),
            "completed preflight request sent a Drop cancel: {first_line:?}"
        );
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    fn prepared_test_tool_call(
        canonical_tool_name: String,
        consent: Option<String>,
    ) -> PreparedModToolCall {
        PreparedModToolCall::new(
            canonical_tool_name.clone(),
            consent,
            ProjectsConsentFacts::default(),
            format!("resolved:{canonical_tool_name}"),
        )
    }

    #[derive(Default)]
    struct DirectAgentApiSession {
        seen_input: StdMutex<Option<ModAgentSpawnInput>>,
        seen_context: StdMutex<Option<DirectAgentApiContextSnapshot>>,
        retained_left_running: StdMutex<Option<ModAgentSpawnSlotLease>>,
        fail_spawn: AtomicBool,
    }

    #[derive(Clone)]
    struct DirectAgentApiContextSnapshot {
        provenance: lingxi_core::host::subagent_spawn::AgentSpawnProvenance,
        request_cancellation: lingxi_core::host::CancellationToken,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for DirectAgentApiSession {
        fn cwd(&self) -> PathBuf {
            PathBuf::from("/tmp/agent-api")
        }

        fn root(&self) -> PathBuf {
            PathBuf::from("/tmp/agent-api")
        }

        async fn model(&self) -> String {
            "agent-api-model".into()
        }

        async fn id(&self) -> String {
            "agent-api-session".into()
        }

        async fn turns(&self) -> u64 {
            0
        }

        async fn agent_list(&self) -> Result<Utf16JsonProjection, ModError> {
            Ok(test_projection(json!([{"agentId":"child-current"}])))
        }

        async fn agent_spawn_api(
            &self,
            input: ModAgentSpawnInput,
            context: ModAgentSpawnContext,
        ) -> Result<Utf16JsonProjection, ModError> {
            if self.fail_spawn.load(Ordering::Acquire) {
                return Err(ModError::Hook("Agent admission refused".into()));
            }
            let retained_left_running = context.retain_left_running_lease()?;
            *self
                .seen_input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(input);
            *self
                .seen_context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(DirectAgentApiContextSnapshot {
                    provenance: context.provenance,
                    request_cancellation: context.request_cancellation,
                });
            *self
                .retained_left_running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(retained_left_running);
            Ok(test_projection(
                json!({"result":{"agentId":"child-launched"},"text":""}),
            ))
        }
    }

    #[tokio::test]
    async fn direct_agent_apis_use_host_context_without_dispatching_duplicate_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("agent-api.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('agent.spawn', () => { throw new Error('agent.spawn is not dispatched twice'); });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "agent-api-plugin",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        assert!(host.has_event("agent.spawn"));

        let session = DirectAgentApiSession::default();
        let api_context = ResolvedModApiContext {
            caller: crate::mod_api_context::ModApiCallerRegistration {
                plugin: "agent-api-plugin".into(),
                storage_id: "agent-api-plugin".into(),
                hook_id: 17,
                event: "turn.step".into(),
            },
            hook_origin: lingxi_core::host::task_registry::FieldPresence::Value(json!([
                "trusted-parent"
            ])),
        };
        let origin = json!({"plugin":"worker-claim-is-ignored"});
        let list = host
            .dispatch_host_api_with_projection(
                "agent.list",
                Some(test_projection(json!({}))),
                dir.path(),
                Some(&session),
                false,
                0,
                origin.clone(),
                api_context.clone(),
                "agent-api-plugin".into(),
                Some(17),
                1,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(list, HostApiValue::JsonProjection(value) if value.semantically_eq(&test_projection(json!([{"agentId":"child-current"}]))))
        );

        let invalid_spawn = host
            .dispatch_host_api_with_projection(
                "agent.spawn",
                Some(test_projection(
                    json!({"tool":"Agent","prompt":"missing required wrapper facts"}),
                )),
                dir.path(),
                Some(&session),
                false,
                0,
                origin.clone(),
                api_context.clone(),
                "agent-api-plugin".into(),
                Some(17),
                1,
                None,
                None,
            )
            .await;
        assert!(invalid_spawn.is_err());
        assert_eq!(host.agent_spawn_budget.running_for("agent-api-plugin"), 0);

        let spawn = host
            .dispatch_host_api_with_projection(
                "agent.spawn",
                Some(test_projection(json!({
                    "tool":"Agent",
                    "prompt":"inspect the current route",
                    "description":"inspect route",
                    "run_in_background":true,
                    "model":null,
                    "subagent_type":"Explore",
                    "name":"route-check",
                    "cwd":"/tmp/agent-api",
                    "ignored":"worker extra",
                }))),
                dir.path(),
                Some(&session),
                false,
                0,
                origin,
                api_context,
                "agent-api-plugin".into(),
                Some(17),
                2,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(spawn, HostApiValue::JsonProjection(value) if value.semantically_eq(&test_projection(json!({"result":{"agentId":"child-launched"},"text":""}))))
        );
        assert_eq!(host.agent_spawn_budget.running_for("agent-api-plugin"), 1);

        let seen_input = session
            .seen_input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        assert_eq!(
            seen_input
                .into_projection()
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"tool":"Agent","prompt":"inspect the current route","description":"inspect route","run_in_background":true,"model":null,"subagent_type":"Explore","name":"route-check","cwd":"/tmp/agent-api"}"#
        );
        let seen_context = session
            .seen_context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap();
        assert_eq!(
            seen_context.provenance.hook_caller,
            lingxi_core::host::task_registry::FieldPresence::Value(Value::String(
                "agent-api-plugin".into()
            ))
        );
        assert_eq!(
            seen_context.provenance.hook_origin,
            lingxi_core::host::task_registry::FieldPresence::Value(json!(["trusted-parent"]))
        );
        assert!(seen_context.request_cancellation.is_cancelled());
        drop(
            session
                .retained_left_running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        assert_eq!(host.agent_spawn_budget.running_for("agent-api-plugin"), 0);

        session.fail_spawn.store(true, Ordering::Release);
        let refused = host
            .dispatch_host_api_with_projection(
                "agent.spawn",
                Some(test_projection(json!({
                    "tool":"Agent",
                    "prompt":"admission refusal",
                    "description":"admission check",
                    "run_in_background":true,
                }))),
                dir.path(),
                Some(&session),
                false,
                0,
                json!({"plugin":"untrusted"}),
                ResolvedModApiContext {
                    caller: crate::mod_api_context::ModApiCallerRegistration {
                        plugin: "agent-api-plugin".into(),
                        storage_id: "agent-api-plugin".into(),
                        hook_id: 17,
                        event: "turn.step".into(),
                    },
                    hook_origin: lingxi_core::host::task_registry::FieldPresence::Value(json!([
                        "trusted-parent"
                    ])),
                },
                "agent-api-plugin".into(),
                Some(17),
                3,
                None,
                None,
            )
            .await;
        assert!(refused.is_err());
        assert_eq!(host.agent_spawn_budget.running_for("agent-api-plugin"), 0);
    }

    struct NotifyTestSessionContext {
        cwd: PathBuf,
        notices: Arc<Mutex<Vec<(String, Utf16JsonProjection)>>>,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for NotifyTestSessionContext {
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }

        fn root(&self) -> PathBuf {
            self.cwd.clone()
        }

        async fn model(&self) -> String {
            "test-model".into()
        }

        async fn id(&self) -> String {
            "notify-session".into()
        }

        async fn turns(&self) -> u64 {
            1
        }

        async fn emit_mod_notify(&self, plugin: &str, text: Utf16JsonProjection) {
            self.notices.lock().await.push((plugin.to_owned(), text));
        }
    }

    struct TestSessionContext {
        cwd: std::sync::Mutex<PathBuf>,
        root: PathBuf,
        model: std::sync::Mutex<String>,
        id: String,
        turns: u64,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for TestSessionContext {
        fn cwd(&self) -> PathBuf {
            self.cwd.lock().unwrap().clone()
        }

        fn root(&self) -> PathBuf {
            self.root.clone()
        }

        fn surfaces(&self) -> Vec<String> {
            if self.id == "surfaces-test" {
                vec!["terminal".into(), "desktop".into()]
            } else {
                Vec::new()
            }
        }

        async fn messages(
            &self,
            input: Option<Utf16JsonProjection>,
        ) -> Result<Utf16JsonProjection, ModError> {
            if input.as_ref().and_then(|input| input.string_at("/as")) == Some("api") {
                return Ok(test_projection(
                    json!([{"role":"user","content":[{"type":"text","text":"hello"}]}]),
                ));
            }
            if input.as_ref().is_none_or(|input| {
                input
                    .at_pointer("")
                    .and_then(JsonValueView::object_entries)
                    .is_some_and(|entries| entries.is_empty())
            }) {
                return Ok(test_projection(
                    json!([{"role":"user","text":"hello","toolUses":[]}]),
                ));
            }
            Err(ModError::Hook("unexpected session.messages input".into()))
        }

        async fn model_fork(
            &self,
            input: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ModError> {
            if self.id == "fork-test" {
                return object_projection(vec![
                    ("isAnswered".into(), static_projection("true")),
                    (
                        "text".into(),
                        input
                            .subprojection("/prompt")
                            .unwrap_or_else(|_| static_projection("null")),
                    ),
                    (
                        "usage".into(),
                        test_projection(json!({
                            "input_tokens":1,
                            "output_tokens":2,
                            "cache_read_input_tokens":3,
                            "cache_creation_input_tokens":4,
                        })),
                    ),
                ]);
            }
            Ok(static_projection(
                r#"{"isAnswered":false,"reason":"nothing-to-fork"}"#,
            ))
        }

        async fn model_complete(
            &self,
            input: Utf16JsonProjection,
            _plugin: &str,
        ) -> Result<Utf16JsonProjection, ModError> {
            if self.id != "complete-test" {
                return Err(ModError::Unavailable("no completion client".into()));
            }
            object_projection(vec![
                ("isAnswered".into(), static_projection("true")),
                (
                    "text".into(),
                    input
                        .subprojection("/prompt")
                        .unwrap_or_else(|_| static_projection("null")),
                ),
                (
                    "usage".into(),
                    test_projection(json!({
                        "input_tokens":1,
                        "output_tokens":2,
                        "cache_read_input_tokens":3,
                        "cache_creation_input_tokens":4,
                    })),
                ),
            ])
        }

        async fn model_classify(
            &self,
            input: lingxi_core::types::utf16_json::Utf16JsonProjection,
            _plugin: &str,
        ) -> Result<lingxi_core::types::utf16_json::Utf16JsonProjection, ModError> {
            if self.id != "classify-test" {
                return Err(ModError::Unavailable("no classifier".into()));
            }
            match input.string_at("/text") {
                Some("question") => Ok(root_string_projection("bug")),
                Some("lone-label") => {
                    if input.string_units("/labels/0") != Some(vec![0xd800]) {
                        return Err(ModError::Hook(
                            "model.classify lost the exact first label".into(),
                        ));
                    }
                    projection_from_string_units(&[0xd800])
                        .map_err(|error| ModError::Protocol(error.to_string()))
                }
                _ => Ok(static_projection("null")),
            }
        }

        async fn settings_read(
            &self,
            input: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ModError> {
            if self.id == "policy-read-error" {
                return Err(ModError::Unavailable("policy source unavailable".into()));
            }
            if self.id == "managed-mods-only" {
                return Ok(test_projection(
                    json!({"pluginConfigs":{"cc-plugin-sec-default@builtin":{
                        "options":{"allowManagedModsOnly":true}
                    }}}),
                ));
            }
            if self.id == "allow-override" {
                return Ok(test_projection(
                    json!({"pluginConfigs":{"cc-plugin-sec-default@builtin":{
                        "options":{"allowModsToOverrideDenyRules":true}
                    }}}),
                ));
            }
            if self.id == "org-tools" {
                return Ok(test_projection(json!({
                    "allowedMcpServers":[{"serverName":"org"}],
                    "managedMcpServers":{"audit":{}}
                })));
            }
            object_projection(vec![(
                "source".into(),
                input
                    .subprojection("/source")
                    .unwrap_or_else(|_| static_projection("null")),
            )])
        }

        async fn tool_list(&self) -> Result<Utf16JsonProjection, ModError> {
            Ok(test_projection(json!([
                {"name":"Read","description":"Read files","mcp":false},
                {"name":"mcp__org__safe","description":"Managed tool","mcp":true},
                {"name":"mcp__audit__log","description":"Managed audit","mcp":true},
                {"name":"mcp__user__old","description":"User tool","mcp":true}
            ])))
        }

        async fn command_list(&self) -> Result<Utf16JsonProjection, ModError> {
            Ok(test_projection(
                json!([{"name":"help","description":"Show help","source":"builtin"}]),
            ))
        }

        async fn command_register(
            &self,
            _plugin: &str,
            spec: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ModError> {
            object_projection(vec![(
                "command".into(),
                spec.subprojection("/name")
                    .unwrap_or_else(|_| static_projection("null")),
            )])
        }

        async fn command_run(
            &self,
            plugin: &str,
            command: Utf16JsonProjection,
            args: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ModError> {
            let mut units = plugin.encode_utf16().collect::<Vec<_>>();
            units.push(b':' as u16);
            units.extend_from_slice(&command.string_units("").unwrap_or_default());
            units.push(b':' as u16);
            units.extend_from_slice(&args.string_units("").unwrap_or_default());
            object_projection(vec![("text".into(), projection_from_string_units(&units)?)])
        }

        async fn prompt_submit(
            &self,
            plugin: &str,
            text: Utf16JsonProjection,
            as_user: bool,
        ) -> Result<Utf16JsonProjection, ModError> {
            if self.id != "submit-test" {
                return Err(ModError::Unavailable("no prompt queue".into()));
            }
            object_projection(vec![
                ("text".into(), text),
                ("plugin".into(), root_string_projection(plugin)),
                (
                    "asUser".into(),
                    static_projection(if as_user { "true" } else { "false" }),
                ),
            ])
        }

        async fn tool_register(
            &self,
            plugin: &str,
            spec: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ModError> {
            let name = spec.string_at("/name").unwrap_or("unknown");
            let mut fields = vec![(
                "tool".into(),
                root_string_projection(&format!("mcp__{plugin}__{name}")),
            )];
            if self.id == "tool-register-is-deferred" {
                if let Ok(input_schema) = spec.subprojection("/inputSchema") {
                    fields.push(("inputSchema".into(), input_schema));
                }
                if let Ok(is_deferred) = spec.subprojection("/isDeferred") {
                    fields.push(("isDeferred".into(), is_deferred));
                }
            }
            object_projection(fields)
        }

        async fn model(&self) -> String {
            self.model.lock().unwrap().clone()
        }

        async fn id(&self) -> String {
            self.id.clone()
        }

        async fn turns(&self) -> u64 {
            self.turns
        }

        async fn version(&self) -> Result<Utf16JsonProjection, ModError> {
            Ok(test_projection(json!({"version":"0.12.0","base":"0.12.0"})))
        }

        async fn tool_check(
            &self,
            _tool: &str,
            _input: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ModError> {
            Ok(test_projection(
                json!({"decision":"ask","reason":"test policy"}),
            ))
        }

        async fn fs_ancestors(
            &self,
            input: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, ModError> {
            let dir = root_string_projection(&self.root.to_string_lossy());
            let name = input
                .subprojection("/names/0")
                .unwrap_or_else(|_| static_projection("null"));
            let entry = object_projection(vec![
                ("dir".into(), dir),
                ("name".into(), name),
                ("content".into(), root_string_projection("memory")),
                ("parts".into(), static_projection("[]")),
            ])?;
            Utf16JsonProjection::array(vec![entry])
                .map_err(|error| ModError::Protocol(error.to_string()))
        }
    }

    #[tokio::test]
    async fn turn_complete_intercepts_answer_and_reports_matching_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("complete.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.complete', { reason: 'answer' }, async ($, e, next) => {
                const core = await next({ ...e, answer: e.answer + ' rewritten' });
                if (core.text !== 'original rewritten') throw new Error('core did not see rewrite');
                return { text: 'short summary', usage: core.usage };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "complete-mod",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "complete-test".into(),
            turns: 1,
        };
        let outcome = host
            .dispatch_with_ui_meta_at_session(
                "turn.complete",
                test_projection(
                    json!({"answer":"original","durationMs":12,"isAborted":false,
                    "turnId":"turn-1","reason":"answer"}),
                ),
                &session,
                |event| async move {
                    let answer = event
                        .and_then(|event| event.subprojection("/answer").ok())
                        .unwrap_or_else(|| static_projection("null"));
                    Ok(Some(
                        object_projection(vec![("text".into(), answer)]).unwrap(),
                    ))
                },
                |_, _| async {},
                |_, _, _| async {},
                |_, _| async {},
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.result_projection.string_at("/text"),
            Some("short summary")
        );
        assert_eq!(outcome.hooked, vec!["complete-mod".to_string()]);
    }

    #[tokio::test]
    async fn turn_step_stream_forwards_model_and_yields_rewritten_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("step.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.step', async function* ($, e, next) {
                const below = next({ ...e, model: 'rewritten-model' });
                for await (const chunk of below) {
                  yield chunk.kind === 'text' ? { ...chunk, text: chunk.text.toUpperCase() } : chunk;
                }
                return { ...await below.result, answer: 'HELLO' };
              });
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("step-mod", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let forwarded = Arc::new(StdMutex::new(Vec::new()));
        let emitted = Arc::new(StdMutex::new(Vec::new()));
        let observed_input = forwarded.clone();
        let observed_output = emitted.clone();
        let result = host
            .dispatch_turn_step_stream(
                test_projection(json!({"turnId":"turn-1","index":0,"model":"original-model","messageCount":1})),
                move |input| {
                    observed_input.lock().unwrap().push(input);
                    async move {
                        Ok(ModStreamSource::new(
                            futures_util::stream::iter(vec![
                                Ok(projected(json!({"kind":"text","index":0,"text":"hello","ref":1}))),
                                Ok(projected(json!({"kind":"stop","stopReason":"end_turn","usage":null,"ref":2}))),
                            ]),
                            || Ok(projected(json!({"turnId":"turn-1","index":0,"answer":"hello","toolUses":[],"stopReason":"end_turn","usage":null}))),
                        ))
                    }
                },
                move |chunk| {
                    observed_output.lock().unwrap().push(chunk);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap();
        assert_eq!(
            forwarded.lock().unwrap()[0].string_at("/model"),
            Some("rewritten-model")
        );
        assert_eq!(emitted.lock().unwrap()[0].string_at("/text"), Some("HELLO"));
        assert_eq!(emitted.lock().unwrap()[1].string_at("/kind"), Some("stop"));
        assert_eq!(result.string_at("/answer"), Some("HELLO"));
    }

    #[tokio::test]
    async fn turn_step_stream_source_and_result_preserve_utf16_projections() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("exact-step.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.step', async function* ($, e, next) {
                const below = next(e);
                for await (const chunk of below) {
                  if (chunk.kind === 'text' && chunk.text.charCodeAt(0) !== 0xd800) {
                    throw new Error('source UTF-16 text was lost');
                  }
                  yield chunk;
                }
                const result = await below.result;
                if (result.answer.charCodeAt(0) !== 0xd800) {
                  throw new Error('result UTF-16 text was lost');
                }
                return result;
              });
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "exact-step",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let emitted = Arc::new(StdMutex::new(Vec::new()));
        let observed = emitted.clone();
        let result = host
            .dispatch_turn_step_stream(
                test_projection(json!({"turnId":"turn-exact","index":0,"model":"model","messageCount":1})),
                |_| async {
                    Ok(ModStreamSource::new(
                        futures_util::stream::iter(vec![Ok(mod_test_projection(
                            r#"{"kind":"text","index":0,"text":"\ud800","ref":1}"#,
                        ))]),
                        || Ok(mod_test_projection(
                            r#"{"turnId":"turn-exact","index":0,"answer":"\ud800","toolUses":[],"stopReason":"end_turn","usage":null}"#,
                        )),
                    ))
                },
                move |chunk| {
                    observed.lock().unwrap().push(chunk);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap();

        assert_eq!(
            emitted.lock().unwrap()[0].string_units("/text"),
            Some(vec![0xd800])
        );
        assert_eq!(result.string_units("/answer"), Some(vec![0xd800]));
    }

    #[tokio::test]
    async fn turn_step_can_answer_without_opening_a_model_stream() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("synthetic-step.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.step', async function* ($, e) {
                yield { kind: 'text', index: 0, text: 'synthetic' };
                yield { kind: 'stop', stopReason: 'end_turn', usage: null };
                return { turnId: e.turnId, index: e.index, answer: 'synthetic',
                  toolUses: [], stopReason: 'end_turn', usage: null };
              });
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "synthetic-step",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        assert!(host.has_event("turn.step"));
        let emitted = Arc::new(StdMutex::new(Vec::new()));
        let observed = emitted.clone();
        let result = host
            .dispatch_turn_step_stream(
                test_projection(
                    json!({"turnId":"turn-2","index":1,"model":"original","messageCount":2}),
                ),
                |_| async { panic!("synthetic response must not open the model") },
                move |chunk| {
                    observed.lock().unwrap().push(chunk);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap();
        assert_eq!(result.string_at("/answer"), Some("synthetic"));
        assert_eq!(emitted.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn turn_step_rejects_forged_engine_ref_before_opening_core_stream() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("forged-step.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.step', async function* () {
                yield { kind: 'engine', ref: 999 };
              });
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "forged-step",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let emitted = Arc::new(StdMutex::new(Vec::new()));
        let observed = emitted.clone();
        let source_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_source_calls = source_calls.clone();
        let result = host
            .dispatch_turn_step_stream(
            test_projection(json!({"turnId":"turn-3","index":0,"model":"original","messageCount":1})),
                move |_| {
                    observed_source_calls.fetch_add(1, Ordering::SeqCst);
                    async {
                    Ok(ModStreamSource::new(
                        futures_util::stream::iter(vec![Ok(projected(json!({"kind":"engine","ref":1})))]),
                        || Ok(projected(json!({"turnId":"turn-3","index":0,"answer":"","toolUses":[],"stopReason":null,"usage":null}))),
                    ))
                    }
                },
                move |chunk| {
                    observed.lock().unwrap().push(chunk);
                    async { Ok(()) }
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(result, ModError::Hook(message)
            if message.contains("turn.step yielded an engine ref this link never pulled as engine")));
        assert_eq!(source_calls.load(Ordering::SeqCst), 0);
        assert!(emitted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn turn_step_trace_records_downstream_hook_and_core_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer-step.js");
        let inner = dir.path().join("inner-step.js");
        std::fs::write(
            &outer,
            r#"
            export function register(on) {
              on('turn.step', async function* ($, e, next) {
                const below = next(e);
                for await (const chunk of below) yield chunk;
                return { ...await below.result, trace: next.trace.map(
                  row => [row.plugin, row.tier, row.outcome, row.chunks]) };
              });
            }
        "#,
        )
        .unwrap();
        std::fs::write(
            &inner,
            r#"
            export function register(on) {
              on('turn.step', async function* ($, e, next) {
                const below = next(e);
                for await (const chunk of below) yield chunk;
                return await below.result;
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "outer-step",
            dir.path(),
            &outer,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        host.load_with_tier(
            "inner-step",
            dir.path(),
            &inner,
            test_mod_options(json!({})),
            "append",
        )
        .await
        .unwrap();
        let result = host.dispatch_turn_step_stream(
            test_projection(json!({"turnId":"turn-4","index":0,"model":"model","messageCount":1})),
            |_| async {
                Ok(ModStreamSource::new(
                    futures_util::stream::iter(vec![Ok(projected(json!({"kind":"text","index":0,"text":"ok","ref":1})))]),
                    || Ok(projected(json!({"turnId":"turn-4","index":0,"answer":"ok","toolUses":[],"stopReason":"end_turn","usage":null}))),
                ))
            },
            |_| async { Ok(()) },
        ).await.unwrap();
        assert_eq!(
            result
                .subprojection("/trace")
                .unwrap()
                .to_json_string()
                .unwrap(),
            test_projection(json!([
                ["inner-step", "append", "passed", 1],
                ["engine", "core", "returned", 1]
            ]))
            .to_json_string()
            .unwrap()
        );
    }

    #[tokio::test]
    async fn turn_step_next_to_skips_user_tier_in_trace() {
        let dir = tempfile::tempdir().unwrap();
        let prepend = dir.path().join("prepend.js");
        let user = dir.path().join("user.js");
        std::fs::write(
            &prepend,
            r#"
            export function register(on) {
              on('turn.step', async function* ($, e, next) {
                const below = next.to(e, 'append');
                for await (const chunk of below) yield chunk;
                return { ...await below.result, trace: next.trace.map(row =>
                  [row.plugin, row.outcome]) };
              });
            }
        "#,
        )
        .unwrap();
        std::fs::write(
            &user,
            r#"
            export function register(on) {
              on('turn.step', async function* () { throw new Error('user tier was not skipped'); });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load_with_tier(
            "outer",
            dir.path(),
            &prepend,
            test_mod_options(json!({})),
            "prepend",
        )
        .await
        .unwrap();
        host.load(
            "skipped-user",
            dir.path(),
            &user,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host.dispatch_turn_step_stream(
            test_projection(json!({"turnId":"turn-5","index":0,"model":"model","messageCount":1})),
            |_| async {
                Ok(ModStreamSource::new(
                    futures_util::stream::iter(vec![Ok(projected(json!({"kind":"text","index":0,"text":"ok","ref":1})))]),
                    || Ok(projected(json!({"turnId":"turn-5","index":0,"answer":"ok","toolUses":[],"stopReason":"end_turn","usage":null}))),
                ))
            },
            |_| async { Ok(()) },
        ).await.unwrap();
        assert_eq!(
            result
                .subprojection("/trace")
                .unwrap()
                .to_json_string()
                .unwrap(),
            test_projection(json!([["skipped-user", "skipped"], ["engine", "returned"]]))
                .to_json_string()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn tool_call_api_reuses_transaction_across_next_runs() {
        struct ToolCallSession {
            cwd: PathBuf,
            preparations: std::sync::Mutex<Vec<(String, Option<String>)>>,
            calls: std::sync::Mutex<Vec<Value>>,
            prepared_handles: std::sync::Mutex<Vec<Arc<PreparedModToolCall>>>,
            transaction_ids: std::sync::Mutex<Vec<ModToolCallTransactionId>>,
            virtual_tool_use_ids: std::sync::Mutex<Vec<String>>,
            virtual_assistant_uuids: std::sync::Mutex<Vec<String>>,
            completed: std::sync::Mutex<Vec<ModToolCallTransactionId>>,
            aborted: std::sync::Mutex<Vec<ModToolCallTransactionId>>,
        }
        #[async_trait::async_trait]
        impl ModSessionContext for ToolCallSession {
            fn cwd(&self) -> PathBuf {
                self.cwd.clone()
            }
            fn root(&self) -> PathBuf {
                self.cwd.clone()
            }
            async fn model(&self) -> String {
                "test".into()
            }
            async fn id(&self) -> String {
                "tool-call-api-test".into()
            }
            async fn turns(&self) -> u64 {
                1
            }
            async fn prepare_tool_call(
                &self,
                plugin: &str,
                requested_tool_name: String,
                consent: Option<String>,
                _cancellation: lingxi_core::host::CancellationToken,
            ) -> Result<PreparedModToolCall, ModError> {
                assert_eq!(plugin, "tool-call-api");
                self.preparations
                    .lock()
                    .unwrap()
                    .push((requested_tool_name.clone(), consent.clone()));
                Ok(prepared_test_tool_call(requested_tool_name, consent))
            }
            async fn tool_call(
                &self,
                plugin: &str,
                input: Utf16JsonProjection,
                context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let input = mod_test_value(&input);
                assert_eq!(plugin, "tool-call-api");
                assert!(matches!(
                    &context.agent_spawn_provenance.hook_caller,
                    lingxi_core::host::task_registry::FieldPresence::Value(value)
                        if value == &json!("tool-call-api")
                ));
                assert!(matches!(
                    &context.agent_spawn_provenance.hook_origin,
                    lingxi_core::host::task_registry::FieldPresence::Value(value)
                        if value == &json!(["host-parent", "tool-call-api"])
                ));
                assert_eq!(
                    context.prepared.projects_consent(),
                    &ProjectsConsentFacts::default()
                );
                assert_eq!(context.prepared.canonical_tool_name(), "Read");
                assert_eq!(context.prepared.consent(), None);
                assert_eq!(
                    context
                        .prepared
                        .resolved_tool::<String>()
                        .map(String::as_str),
                    Some("resolved:Read")
                );
                assert!(context.virtual_tool_use_id.starts_with("toolu_plugin_"));
                assert_eq!(context.virtual_tool_use_id.len(), 45);
                assert!(context.virtual_tool_use_id[13..]
                    .chars()
                    .all(|character| character.is_ascii_hexdigit()));
                assert_ne!(
                    context.virtual_tool_use_id.as_str(),
                    input["tool_use_id"].as_str().unwrap()
                );
                assert_eq!(context.virtual_assistant_uuid.len(), 36);
                assert_eq!(
                    context
                        .virtual_assistant_uuid
                        .split('-')
                        .map(str::len)
                        .collect::<Vec<_>>(),
                    vec![8, 4, 4, 4, 12]
                );
                self.calls.lock().unwrap().push(input.clone());
                self.prepared_handles
                    .lock()
                    .unwrap()
                    .push(context.prepared.clone());
                self.transaction_ids
                    .lock()
                    .unwrap()
                    .push(context.transaction_id);
                self.virtual_tool_use_ids
                    .lock()
                    .unwrap()
                    .push(context.virtual_tool_use_id.clone());
                self.virtual_assistant_uuids
                    .lock()
                    .unwrap()
                    .push(context.virtual_assistant_uuid.clone());
                Ok(test_projection(json!({
                    "result":{"path":input["path"]},
                    "text":"core result"
                })))
            }

            async fn complete_tool_call(
                &self,
                transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.completed.lock().unwrap().push(transaction_id);
                Ok(())
            }

            async fn abort_tool_call(
                &self,
                transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.aborted.lock().unwrap().push(transaction_id);
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("tool-call.js");
        std::fs::write(
            &module,
            r#"
            let hookInvocations = 0;
            export function register(on) {
              on('turn.start', async ($, e, next) => {
                if (JSON.stringify(next.origin) !== '["host-parent","tool-call-api"]') {
                  throw new Error('host origin was not visible to the hook');
                }
                try {
                  const answer = await $.tool.call({
                      tool: 'Read', path: '/original', tool_use_id: 'caller-id-is-not-reused'
                    });
                  return { turnId: e.turnId, answer, answerKeys: Object.keys(answer) };
                } catch (error) {
                  return { turnId: e.turnId, apiError: String(error) };
                }
              });
              on('tool.call', async ($, e, next) => {
                hookInvocations += 1;
                await next({ ...e, path: '/rewritten' });
                const answer = await next({ ...e, path: '/second' });
                return { ...answer, text: `${answer.text}:${hookInvocations}` };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "tool-call-api",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = ToolCallSession {
            cwd: dir.path().to_path_buf(),
            preparations: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::Mutex::new(Vec::new()),
            prepared_handles: std::sync::Mutex::new(Vec::new()),
            transaction_ids: std::sync::Mutex::new(Vec::new()),
            virtual_tool_use_ids: std::sync::Mutex::new(Vec::new()),
            virtual_assistant_uuids: std::sync::Mutex::new(Vec::new()),
            completed: std::sync::Mutex::new(Vec::new()),
            aborted: std::sync::Mutex::new(Vec::new()),
        };
        let answer = host
            .dispatch_with_ui_at_session_cwd_and_origin(
                "turn.start",
                test_projection(json!({"turnId":"turn-1"})),
                &session,
                &session.cwd,
                lingxi_core::host::task_registry::FieldPresence::Value(json!([
                    "host-parent",
                    "tool-call-api"
                ])),
                |event| async move { Ok(event) },
                |_, _| async {},
                |_, _, _| async {},
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        let answer_value = mod_test_value(&answer);

        assert_eq!(
            answer_value["answer"]["result"]["path"],
            "/second",
            "answer={answer:?}; calls={:?}",
            *session.calls.lock().unwrap()
        );
        assert_eq!(
            answer_value["answer"]["text"], "core result:1",
            "one middleware invocation calls next twice; its second core answer is returned"
        );
        assert_eq!(answer_value["answerKeys"], json!(["result", "text"]));
        assert_eq!(answer_value["answer"].get("apiError"), None);
        assert_eq!(
            *session.preparations.lock().unwrap(),
            vec![("Read".into(), None)],
            "the host resolves once before entering middleware"
        );
        assert_eq!(
            *session.calls.lock().unwrap(),
            vec![
                json!({"tool":"Read","path":"/rewritten","tool_use_id":"caller-id-is-not-reused"}),
                json!({"tool":"Read","path":"/second","tool_use_id":"caller-id-is-not-reused"})
            ]
        );
        let transaction_ids = session.transaction_ids.lock().unwrap();
        assert_eq!(transaction_ids.len(), 2);
        assert_eq!(transaction_ids[0], transaction_ids[1]);
        let prepared_handles = session.prepared_handles.lock().unwrap();
        assert_eq!(prepared_handles.len(), 2);
        assert!(Arc::ptr_eq(&prepared_handles[0], &prepared_handles[1]));
        let virtual_tool_use_ids = session.virtual_tool_use_ids.lock().unwrap();
        assert_eq!(virtual_tool_use_ids.len(), 2);
        assert_eq!(virtual_tool_use_ids[0], virtual_tool_use_ids[1]);
        let virtual_assistant_uuids = session.virtual_assistant_uuids.lock().unwrap();
        assert_eq!(virtual_assistant_uuids.len(), 2);
        assert_eq!(virtual_assistant_uuids[0], virtual_assistant_uuids[1]);
        let completed = session.completed.lock().unwrap();
        assert_eq!(completed.as_slice(), &[transaction_ids[0]]);
        assert!(session.aborted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tool_call_prepares_before_middleware_and_preserves_empty_consent() {
        struct PreflightSession {
            cwd: PathBuf,
            preparations: std::sync::Mutex<Vec<(String, Option<String>)>>,
            projects_checks: std::sync::atomic::AtomicUsize,
            core_calls: std::sync::atomic::AtomicUsize,
            completed: std::sync::atomic::AtomicUsize,
            aborted: std::sync::atomic::AtomicUsize,
        }

        #[async_trait::async_trait]
        impl ModSessionContext for PreflightSession {
            fn cwd(&self) -> PathBuf {
                self.cwd.clone()
            }

            fn root(&self) -> PathBuf {
                self.cwd.clone()
            }

            async fn model(&self) -> String {
                "test".into()
            }

            async fn id(&self) -> String {
                "tool-call-preflight-test".into()
            }

            async fn turns(&self) -> u64 {
                1
            }

            async fn prepare_tool_call(
                &self,
                plugin: &str,
                requested_tool_name: String,
                consent: Option<String>,
                _cancellation: lingxi_core::host::CancellationToken,
            ) -> Result<PreparedModToolCall, ModError> {
                assert_eq!(plugin, "tool-call-preflight");
                self.preparations
                    .lock()
                    .unwrap()
                    .push((requested_tool_name.clone(), consent.clone()));
                if requested_tool_name == "missing" {
                    return Err(ModError::Hook(
                        "tool-call-preflight: $.tool.call: no tool named \"missing\" in this session"
                            .into(),
                    ));
                }
                let canonical_tool_name = match requested_tool_name.as_str() {
                    "fetch" => "WebFetch",
                    "read-alias" => "Read",
                    other => panic!("unexpected prepared tool {other}"),
                };
                if consent.is_some() {
                    self.projects_checks
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if canonical_tool_name == "WebFetch" {
                        return Err(ModError::Hook(
                            "WebFetch does not accept `consent` in a Projects session".into(),
                        ));
                    }
                }
                Ok(PreparedModToolCall::new(
                    canonical_tool_name.into(),
                    consent,
                    ProjectsConsentFacts::default(),
                    format!("resolved:{canonical_tool_name}"),
                ))
            }

            async fn tool_call(
                &self,
                plugin: &str,
                input: Utf16JsonProjection,
                context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let input = mod_test_value(&input);
                assert_eq!(plugin, "tool-call-preflight");
                assert_eq!(input["tool"], "read-alias");
                assert_eq!(input["path"], "/prepared");
                assert_eq!(context.prepared.canonical_tool_name(), "Read");
                assert_eq!(context.prepared.consent(), Some(""));
                assert_eq!(
                    context
                        .prepared
                        .resolved_tool::<String>()
                        .map(String::as_str),
                    Some("resolved:Read")
                );
                self.core_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(test_projection(
                    json!({"result":{"path":input["path"]},"text":"prepared"}),
                ))
            }

            async fn complete_tool_call(
                &self,
                _transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.completed
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }

            async fn abort_tool_call(
                &self,
                _transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.aborted
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("tool-call-preflight.js");
        std::fs::write(
            &module,
            r#"
            let middlewareCalls = 0;
            export function register(on) {
              on('turn.start', async ($, e) => {
                const shapeError = await $.tool.call({ tool: '', consent: null })
                  .then(() => '', error => String(error));
                const consentError = await $.tool.call({ tool: 'missing', consent: null })
                  .then(() => '', error => error.message);
                const unknownError = await $.tool.call({ tool: 'missing', consent: 'human' })
                  .then(() => '', error => String(error));
                const projectsError = await $.tool.call({ tool: 'fetch', consent: 'human' })
                  .then(() => '', error => String(error));
                const answer = await $.tool.call({
                  tool: 'read-alias', consent: '', path: '/original'
                });
                return { turnId: e.turnId, shapeError, consentError, unknownError,
                  projectsError, answer, middlewareCalls };
              });
              on('tool.call', async ($, e, next) => {
                if (e.tool !== 'read-alias') throw new Error('unexpected middleware identity');
                middlewareCalls += 1;
                return next({ ...e, path: '/prepared' });
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "tool-call-preflight",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = PreflightSession {
            cwd: dir.path().to_path_buf(),
            preparations: std::sync::Mutex::new(Vec::new()),
            projects_checks: std::sync::atomic::AtomicUsize::new(0),
            core_calls: std::sync::atomic::AtomicUsize::new(0),
            completed: std::sync::atomic::AtomicUsize::new(0),
            aborted: std::sync::atomic::AtomicUsize::new(0),
        };
        let answer = host
            .dispatch_with_log_at_session(
                "turn.start",
                test_projection(json!({"turnId":"preflight-turn"})),
                &session,
                |event| async move { Ok(event) },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);

        assert!(answer["shapeError"]
            .as_str()
            .unwrap()
            .contains("takes the event's input"));
        assert_eq!(
            answer["consentError"],
            "tool-call-preflight: $.tool.call: consent, when given, is a string",
            "the public tool.call API preserves Native's error message without Rust Display context"
        );
        assert!(answer["unknownError"]
            .as_str()
            .unwrap()
            .contains("no tool named \"missing\""));
        assert!(answer["projectsError"]
            .as_str()
            .unwrap()
            .contains("WebFetch does not accept `consent` in a Projects session"));
        assert_eq!(answer["answer"]["result"]["path"], "/prepared");
        assert_eq!(answer["answer"]["text"], "prepared");
        assert_eq!(answer["middlewareCalls"], 1);
        assert_eq!(
            *session.preparations.lock().unwrap(),
            vec![
                ("missing".into(), Some("human".into())),
                ("fetch".into(), Some("human".into())),
                ("read-alias".into(), Some("".into())),
            ],
            "invalid tool/consent inputs must fail before session preparation; empty consent remains present"
        );
        assert_eq!(
            session.projects_checks.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "unknown names fail before the consent gate, while both non-null consent strings—including empty—reach it"
        );
        assert_eq!(
            session.core_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "preflight errors cannot reach tool.call core"
        );
        assert_eq!(
            session.completed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only the prepared call owns a completion lifecycle"
        );
        assert_eq!(session.aborted.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tool_call_ignores_the_non_native_second_argument() {
        struct CancelledToolSession {
            cwd: PathBuf,
            started: std::sync::atomic::AtomicBool,
            observed_cancellation: std::sync::atomic::AtomicBool,
            completed: std::sync::Mutex<Vec<ModToolCallTransactionId>>,
            aborted: std::sync::Mutex<Vec<ModToolCallTransactionId>>,
        }

        #[async_trait::async_trait]
        impl ModSessionContext for CancelledToolSession {
            fn cwd(&self) -> PathBuf {
                self.cwd.clone()
            }

            fn root(&self) -> PathBuf {
                self.cwd.clone()
            }

            async fn model(&self) -> String {
                "test".into()
            }

            async fn id(&self) -> String {
                "tool-call-cancel-test".into()
            }

            async fn turns(&self) -> u64 {
                1
            }

            async fn prepare_tool_call(
                &self,
                plugin: &str,
                requested_tool_name: String,
                consent: Option<String>,
                _cancellation: lingxi_core::host::CancellationToken,
            ) -> Result<PreparedModToolCall, ModError> {
                assert_eq!(plugin, "tool-call-cancel");
                assert_eq!(requested_tool_name, "SlowRead");
                assert_eq!(consent, None);
                Ok(prepared_test_tool_call(requested_tool_name, consent))
            }

            async fn tool_list(&self) -> Result<Utf16JsonProjection, ModError> {
                let mut tools = vec![json!({
                    "name":"Read",
                    "description":"Read files",
                    "mcp":false
                })];
                if self.started.load(std::sync::atomic::Ordering::SeqCst) {
                    tools.push(json!({
                        "name":"SlowRead",
                        "description":"The pending test tool",
                        "mcp":false
                    }));
                }
                Ok(test_projection(Value::Array(tools)))
            }

            async fn tool_call(
                &self,
                plugin: &str,
                input: Utf16JsonProjection,
                context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let input = mod_test_value(&input);
                assert_eq!(plugin, "tool-call-cancel");
                assert_eq!(input["tool"], "SlowRead");
                self.started
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.observed_cancellation.store(
                    context.cancellation.is_cancelled(),
                    std::sync::atomic::Ordering::SeqCst,
                );
                Ok(test_projection(
                    json!({"result":"read-result","text":"read-result"}),
                ))
            }

            async fn complete_tool_call(
                &self,
                transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.completed.lock().unwrap().push(transaction_id);
                Ok(())
            }

            async fn abort_tool_call(
                &self,
                transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.aborted.lock().unwrap().push(transaction_id);
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("tool-call-cancel.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.start', async ($, e) => {
                const answer = await $.tool.call(
                  { tool: 'SlowRead' }, { signal: 'Native ignores this argument' });
                return { turnId: e.turnId, answer, apiKeys: Object.keys(answer) };
              });
              on('tool.call', ($, e, next) => next(e));
            }
        "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        host.load(
            "tool-call-cancel",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = CancelledToolSession {
            cwd: dir.path().to_path_buf(),
            started: std::sync::atomic::AtomicBool::new(false),
            observed_cancellation: std::sync::atomic::AtomicBool::new(false),
            completed: std::sync::Mutex::new(Vec::new()),
            aborted: std::sync::Mutex::new(Vec::new()),
        };
        let answer = host
            .dispatch_with_log_at_session(
                "turn.start",
                test_projection(json!({"turnId":"cancel-turn"})),
                &session,
                |event| async move { Ok(event) },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);

        assert_eq!(
            answer["answer"],
            json!({"result":"read-result","text":"read-result"})
        );
        assert_eq!(answer["apiKeys"], json!(["result", "text"]));
        assert!(session.started.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!session
            .observed_cancellation
            .load(std::sync::atomic::Ordering::SeqCst));
        let completed = session.completed.lock().unwrap();
        assert_eq!(completed.len(), 1);
        assert!(session.aborted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tool_call_api_projects_agent_error_after_strict_middleware_validation() {
        struct ProjectionSession {
            cwd: PathBuf,
            phases: std::sync::Mutex<Vec<&'static str>>,
        }

        #[async_trait::async_trait]
        impl ModSessionContext for ProjectionSession {
            fn cwd(&self) -> PathBuf {
                self.cwd.clone()
            }

            fn root(&self) -> PathBuf {
                self.cwd.clone()
            }

            async fn model(&self) -> String {
                "test".into()
            }

            async fn id(&self) -> String {
                "tool-call-projection-test".into()
            }

            async fn turns(&self) -> u64 {
                1
            }

            async fn prepare_tool_call(
                &self,
                plugin: &str,
                requested_tool_name: String,
                consent: Option<String>,
                _cancellation: lingxi_core::host::CancellationToken,
            ) -> Result<PreparedModToolCall, ModError> {
                assert_eq!(plugin, "tool-call-projection");
                Ok(prepared_test_tool_call(requested_tool_name, consent))
            }

            async fn tool_call(
                &self,
                plugin: &str,
                input: Utf16JsonProjection,
                _context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let input = mod_test_value(&input);
                assert_eq!(plugin, "tool-call-projection");
                if input["tool"] == "Invalid" {
                    self.phases.lock().unwrap().push("core-invalid");
                    return Ok(test_projection(json!({
                        "result":"core-invalid",
                        "text":"core invalid"
                    })));
                }
                assert_eq!(input["tool"], "Agent");
                self.phases.lock().unwrap().push("core-agent");
                Ok(test_projection(json!({
                    "result":{"raw":"agent-tool-result"},
                    "text":"agent failed",
                    "isError":true
                })))
            }

            async fn project_tool_call_api_result(
                &self,
                input: Utf16JsonProjection,
                accepted_answer: Utf16JsonProjection,
                context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let input = mod_test_value(&input);
                let accepted_answer = mod_test_value(&accepted_answer);
                if input["tool"] == "Invalid" {
                    assert_eq!(accepted_answer["result"], "core-invalid");
                    assert_eq!(accepted_answer["text"], "core invalid");
                    self.phases.lock().unwrap().push("project-invalid");
                    return Ok(test_projection(accepted_answer));
                }
                assert_eq!(input["tool"], "Agent");
                assert_eq!(input["tool_use_id"], "caller-id-is-not-reused");
                assert_eq!(accepted_answer["result"]["raw"], "agent-tool-result");
                assert_eq!(accepted_answer["text"], "agent failed");
                assert_eq!(accepted_answer["isError"], true);
                assert!(context.virtual_tool_use_id.starts_with("toolu_plugin_"));
                self.phases.lock().unwrap().push("project-agent");
                // Native FAt(undefined, text) is projected after the strict
                // middleware answer has been accepted.
                Ok(test_projection(
                    json!({"text":"agent failed","isError":true}),
                ))
            }

            async fn complete_tool_call(
                &self,
                _transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.phases.lock().unwrap().push("complete");
                Ok(())
            }

            async fn abort_tool_call(
                &self,
                _transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.phases.lock().unwrap().push("abort");
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("tool-call-projection.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.start', async ($, e) => {
                const invalidAnswer = await $.tool.call({ tool: 'Invalid' });
                const answer = await $.tool.call({
                  tool: 'Agent', prompt: 'run', tool_use_id: 'caller-id-is-not-reused'
                });
                return {
                  turnId: e.turnId,
                  invalidAnswer,
                  answer,
                  apiShape: {
                    hasOwnResult: Object.hasOwn(answer, 'result'),
                    resultUndefined: answer.result === undefined,
                    keys: Object.keys(answer),
                  },
                };
              });
              on('tool.call', ($, e, next) => {
                if (e.tool === 'Invalid') return { text: 'invalid', isError: true };
                return next(e);
              });
            }
        "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        host.load(
            "tool-call-projection",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = ProjectionSession {
            cwd: dir.path().to_path_buf(),
            phases: std::sync::Mutex::new(Vec::new()),
        };
        let answer = host
            .dispatch_with_log_at_session(
                "turn.start",
                test_projection(json!({"turnId":"projection-turn"})),
                &session,
                |event| async move { Ok(event) },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);

        assert_eq!(
            answer["invalidAnswer"],
            json!({"result":"core-invalid","text":"core invalid"})
        );
        assert_eq!(
            answer["answer"],
            json!({"text":"agent failed","isError":true})
        );
        assert_eq!(answer["apiShape"]["hasOwnResult"], true);
        assert_eq!(answer["apiShape"]["resultUndefined"], true);
        assert_eq!(
            answer["apiShape"]["keys"],
            json!(["result", "text", "isError"])
        );
        assert_eq!(
            *session.phases.lock().unwrap(),
            vec![
                "core-invalid",
                "project-invalid",
                "complete",
                "core-agent",
                "project-agent",
                "complete"
            ]
        );
    }

    #[tokio::test]
    async fn tool_call_cancellation_waits_for_agent_api_projection_before_settlement() {
        struct CancelProjectionSession {
            cwd: PathBuf,
            started: std::sync::atomic::AtomicBool,
            projection_started: std::sync::Arc<tokio::sync::Notify>,
            projection_release: std::sync::Arc<tokio::sync::Notify>,
            completed: std::sync::atomic::AtomicBool,
            aborted: std::sync::atomic::AtomicBool,
        }

        #[async_trait::async_trait]
        impl ModSessionContext for CancelProjectionSession {
            fn cwd(&self) -> PathBuf {
                self.cwd.clone()
            }

            fn root(&self) -> PathBuf {
                self.cwd.clone()
            }

            async fn model(&self) -> String {
                "test".into()
            }

            async fn id(&self) -> String {
                "tool-call-cancel-projection-test".into()
            }

            async fn turns(&self) -> u64 {
                1
            }

            async fn prepare_tool_call(
                &self,
                plugin: &str,
                requested_tool_name: String,
                consent: Option<String>,
                _cancellation: lingxi_core::host::CancellationToken,
            ) -> Result<PreparedModToolCall, ModError> {
                assert_eq!(plugin, "tool-call-cancel-projection");
                Ok(prepared_test_tool_call(requested_tool_name, consent))
            }

            async fn tool_list(&self) -> Result<Utf16JsonProjection, ModError> {
                let mut tools = vec![json!({
                    "name":"Read",
                    "description":"Read files",
                    "mcp":false
                })];
                if self.started.load(std::sync::atomic::Ordering::SeqCst) {
                    tools.push(json!({
                        "name":"Agent",
                        "description":"The pending Agent test tool",
                        "mcp":false
                    }));
                }
                Ok(test_projection(Value::Array(tools)))
            }

            async fn tool_call(
                &self,
                _plugin: &str,
                input: Utf16JsonProjection,
                context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let input = mod_test_value(&input);
                assert_eq!(input["tool"], "Agent");
                self.started
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                context.cancellation.cancel();
                Ok(test_projection(json!({
                    "result":{"raw":"agent-result"},
                    "text":"agent failed",
                    "isError":true
                })))
            }

            async fn project_tool_call_api_result(
                &self,
                _input: Utf16JsonProjection,
                accepted_answer: Utf16JsonProjection,
                context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let accepted_answer = mod_test_value(&accepted_answer);
                assert_eq!(accepted_answer["result"]["raw"], "agent-result");
                assert!(context.cancellation.is_cancelled());
                self.projection_started.notify_one();
                self.projection_release.notified().await;
                Ok(test_projection(
                    json!({"text":"agent failed","isError":true}),
                ))
            }

            async fn complete_tool_call(
                &self,
                _transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.completed
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }

            async fn abort_tool_call(
                &self,
                _transaction_id: ModToolCallTransactionId,
            ) -> Result<(), ModError> {
                self.aborted
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("tool-call-cancel-projection.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.start', async ($, e) => {
                const answer = await $.tool.call({ tool: 'Agent' });
                return {
                  turnId: e.turnId,
                  answer,
                  apiShape: {
                    hasOwnResult: Object.hasOwn(answer, 'result'),
                    resultUndefined: answer.result === undefined,
                    keys: Object.keys(answer),
                  },
                };
              });
              on('tool.call', ($, e, next) => next(e));
            }
        "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        host.load(
            "tool-call-cancel-projection",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = std::sync::Arc::new(CancelProjectionSession {
            cwd: dir.path().to_path_buf(),
            started: std::sync::atomic::AtomicBool::new(false),
            projection_started: std::sync::Arc::new(tokio::sync::Notify::new()),
            projection_release: std::sync::Arc::new(tokio::sync::Notify::new()),
            completed: std::sync::atomic::AtomicBool::new(false),
            aborted: std::sync::atomic::AtomicBool::new(false),
        });
        let dispatch_host = host.clone();
        let dispatch_session = session.clone();
        let dispatch = tokio::spawn(async move {
            dispatch_host
                .dispatch_with_log_at_session(
                    "turn.start",
                    test_projection(json!({"turnId":"cancel-project-turn"})),
                    dispatch_session.as_ref(),
                    |event| async move { Ok(event) },
                    |_, _| async {},
                )
                .await
        });

        tokio::time::timeout(
            Duration::from_secs(3),
            session.projection_started.notified(),
        )
        .await
        .expect("cancelled Agent result must reach post-middleware projection");
        assert!(session.started.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            !session.completed.load(std::sync::atomic::Ordering::SeqCst),
            "the API call must remain unsettled during projection"
        );
        session.projection_release.notify_one();

        let answer = tokio::time::timeout(Duration::from_secs(3), dispatch)
            .await
            .expect("the projected cancellation result must settle")
            .unwrap()
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(
            answer["answer"],
            json!({"text":"agent failed","isError":true})
        );
        assert_eq!(answer["apiShape"]["hasOwnResult"], true);
        assert_eq!(answer["apiShape"]["resultUndefined"], true);
        assert_eq!(
            answer["apiShape"]["keys"],
            json!(["result", "text", "isError"])
        );
        assert!(session.completed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!session.aborted.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropping_outer_dispatch_cancels_a_started_tool_call_context() {
        struct PendingToolSession {
            cwd: PathBuf,
            started: std::sync::Arc<tokio::sync::Notify>,
            observed_cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }

        #[async_trait::async_trait]
        impl ModSessionContext for PendingToolSession {
            fn cwd(&self) -> PathBuf {
                self.cwd.clone()
            }

            fn root(&self) -> PathBuf {
                self.cwd.clone()
            }

            async fn model(&self) -> String {
                "test".into()
            }

            async fn id(&self) -> String {
                "tool-call-drop-test".into()
            }

            async fn turns(&self) -> u64 {
                1
            }

            async fn prepare_tool_call(
                &self,
                plugin: &str,
                requested_tool_name: String,
                consent: Option<String>,
                _cancellation: lingxi_core::host::CancellationToken,
            ) -> Result<PreparedModToolCall, ModError> {
                assert_eq!(plugin, "tool-call-drop");
                Ok(prepared_test_tool_call(requested_tool_name, consent))
            }

            async fn tool_call(
                &self,
                _plugin: &str,
                _input: Utf16JsonProjection,
                context: &ModToolCallContext,
            ) -> Result<Utf16JsonProjection, ModError> {
                let cancellation = context.cancellation.clone();
                let observed = self.observed_cancellation.clone();
                tokio::spawn(async move {
                    cancellation.cancelled().await;
                    observed.store(true, std::sync::atomic::Ordering::SeqCst);
                });
                self.started.notify_one();
                std::future::pending::<Result<Utf16JsonProjection, ModError>>().await
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("tool-call-drop.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('turn.start', async ($, e) => ({
                turnId: e.turnId,
                answer: await $.tool.call({ tool: 'Pending' }),
              }));
              on('tool.call', ($, e, next) => next(e));
            }
        "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        host.load(
            "tool-call-drop",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let observed_cancellation = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let session = PendingToolSession {
            cwd: dir.path().to_path_buf(),
            started: started.clone(),
            observed_cancellation: observed_cancellation.clone(),
        };
        let dispatch_host = host.clone();
        let blocked = tokio::spawn(async move {
            dispatch_host
                .dispatch_with_log_at_session(
                    "turn.start",
                    test_projection(json!({"turnId":"drop-turn"})),
                    &session,
                    |event| async move { Ok(event) },
                    |_, _| async {},
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .expect("the host tool core must start before dispatch cancellation");
        blocked.abort();
        let _ = blocked.await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while !observed_cancellation.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropping the outer dispatch must signal the active tool core");
    }

    #[tokio::test]
    async fn ui_selection_is_an_interceptable_operation_and_preserves_absence() {
        struct SelectionSession {
            cwd: PathBuf,
            selection: Option<Utf16JsonProjection>,
        }
        #[async_trait::async_trait]
        impl ModSessionContext for SelectionSession {
            fn cwd(&self) -> PathBuf {
                self.cwd.clone()
            }
            fn root(&self) -> PathBuf {
                self.cwd.clone()
            }
            async fn model(&self) -> String {
                "test".into()
            }
            async fn id(&self) -> String {
                "selection-test".into()
            }
            async fn turns(&self) -> u64 {
                1
            }
            async fn ui_selection(&self) -> Result<Option<Utf16JsonProjection>, ModError> {
                Ok(self.selection.clone())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("selection.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('ui.selection', async (_, event, next) => {
                const blocked = await next({ extra: true }).then(
                  () => false, () => true);
                if (Object.keys(event).length !== 0 || !blocked) {
                  throw new Error('ui.selection operation input was not constrained');
                }
                const result = await next(event);
                return result.value === undefined
                  ? result
                  : { value: { ...result.value, source: 'selection-hook' } };
              });
              on('tool.call', async ($) => {
                const selection = await $.ui.selection();
                return { result: { selection, absent: selection === undefined } };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "selection-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        for (selection, expected) in [
            (None, json!({"absent":true})),
            (
                Some(test_projection(
                    json!({"text":"selected","requestId":"req-1"}),
                )),
                json!({"selection":{"text":"selected","requestId":"req-1",
                    "source":"selection-hook"},"absent":false}),
            ),
        ] {
            let session = SelectionSession {
                cwd: dir.path().to_path_buf(),
                selection,
            };
            let result = host
                .dispatch_with_log_at_session(
                    "tool.call",
                    test_projection(json!({"tool":"Selection"})),
                    &session,
                    |_| async { panic!("Mod answers tool.call") },
                    |_, _| async {},
                )
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result["result"], expected);
        }
    }

    #[tokio::test]
    async fn user_telemetry_hooks_must_name_collector() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("telemetry.js");
        let host = ModHost::start(None).await.unwrap();
        for pattern in [
            "on('telemetry.log', (_, e, next) => next(e));",
            "on('telemetry.*', (_, e, next) => next(e));",
            "on('telemetry.mark', { to: ['collector', 'anthropic'] }, (_, e, next) => next(e));",
        ] {
            std::fs::write(
                &module,
                format!("export function register(on) {{ {pattern} }}"),
            )
            .unwrap();
            let error = host
                .load(
                    "telemetry-test",
                    dir.path(),
                    &module,
                    test_mod_options(json!({})),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("name the collector"));
        }
    }

    #[tokio::test]
    async fn telemetry_api_dispatches_hooks_without_emitting_core_events() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("telemetry.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('*', ($, e, next) => {
                if (e?.to === 'collector' || e?.to === 'anthropic') return { deny: 'wildcard saw telemetry' };
                return next(e);
              });
              on('telemetry.log', { to: 'collector' }, ($, e, next) => {
                if (e.to !== 'collector' || e.event !== 'probe') return { deny: 'wrong input' };
                return next(e);
              });
              on('telemetry.mark', { to: 'collector', kind: 'blocked' }, () => ({ deny: 'blocked' }));
              on('tool.call', async ($) => {
                let logged, defaultLog, unhandled;
                try {
                  logged = await $.telemetry.log({ to: 'collector', event: 'probe' });
                  defaultLog = await $.telemetry.log({ event: 'probe' });
                  unhandled = await $.telemetry.mark({ feature: 'test', kind: 'free' });
                } catch (error) { return { result: { fatal: error.message } }; }
                let denied;
                try { await $.telemetry.mark({ to: 'collector', feature: 'test', kind: 'blocked' }); }
                catch (error) { denied = error.message; }
                let invalid;
                try { await $.telemetry.log({ to: 'elsewhere', event: 'probe' }); }
                catch (error) { invalid = error.message; }
                return { result: { loggedIsVoid: logged === undefined,
                  defaultLogIsVoid: defaultLog === undefined,
                  unhandledIsVoid: unhandled === undefined, denied, invalid } };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "telemetry-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Telemetry"})),
                dir.path(),
                |_| async { panic!("Mod answers tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert!(result["result"].get("fatal").is_none(), "{result}");
        assert_eq!(result["result"]["loggedIsVoid"], true);
        assert_eq!(result["result"]["defaultLogIsVoid"], true);
        assert_eq!(result["result"]["unhandledIsVoid"], true);
        assert_eq!(
            result["result"]["denied"],
            "telemetry-test: $.telemetry.mark: blocked"
        );
        assert!(result["result"]["invalid"]
            .as_str()
            .unwrap()
            .contains("to \"anthropic\" or \"collector\""));
    }

    #[tokio::test]
    async fn model_fork_dispatches_rewritten_prompt_and_runtime_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("fork.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('model.fork', ($, e, next) => next({ ...e, prompt: e.prompt + ' rewritten' }));
              on('tool.call', async ($) => ({ result: await $.model.fork({ prompt: 'question' }) }));
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "fork-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "fork-test".into(),
            turns: 1,
        };
        let answer = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Fork"})),
                &session,
                |_| async { panic!("Mod answers tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["result"]["isAnswered"], true);
        assert_eq!(answer["result"]["text"], "question rewritten");
        assert_eq!(answer["result"]["usage"]["cache_read_input_tokens"], 3);

        let cold_session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "cold-test".into(),
            turns: 0,
        };
        let cold = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Fork"})),
                &cold_session,
                |_| async { panic!("Mod answers tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let cold = mod_test_value(&cold);
        assert_eq!(
            cold["result"],
            json!({"isAnswered":false,"reason":"nothing-to-fork"})
        );
    }

    #[tokio::test]
    async fn model_complete_dispatches_rewritten_request_and_preaborted_result() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("complete.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('model.complete', ($, e, next) => next({ ...e, prompt: e.prompt + ' rewritten' }));
              on('tool.call', async ($) => {
                const answer = await $.model.complete({ model: 'claude-sonnet-4-6', prompt: 'question' });
                const controller = new AbortController();
                controller.abort();
                const aborted = await $.model.complete({ model: 'claude-sonnet-4-6', prompt: 'ignored' }, { signal: controller.signal });
                return { result: { answer, aborted } };
              });
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "complete-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "complete-test".into(),
            turns: 1,
        };
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Complete"})),
                &session,
                |_| async { panic!("Mod answers tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"]["answer"]["text"], "question rewritten");
        assert_eq!(result["result"]["answer"]["usage"]["output_tokens"], 2);
        assert_eq!(result["result"]["aborted"]["reason"], "aborted");
        assert_eq!(result["result"]["aborted"]["usage"]["input_tokens"], 0);
    }

    #[tokio::test]
    async fn model_classify_dispatches_and_preserves_undefined_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("classify.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('model.classify', ($, e, next) => next(e.text === 'rewrite' ? { ...e, text: 'question' } : e));
              on('tool.call', async ($) => {
                const label = await $.model.classify('rewrite', ['bug', 'feature']);
                const noMatch = await $.model.classify('none', ['bug', 'feature']);
                const lone = String.fromCharCode(0xd800);
                const exactLabel = await $.model.classify('lone-label', [lone, 'feature']);
                return { result: {
                  label,
                  noMatchIsUndefined: noMatch === undefined,
                  exactLabelPreserved: exactLabel === lone && exactLabel.charCodeAt(0) === 0xd800,
                } };
              });
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "classify-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "classify-test".into(),
            turns: 1,
        };
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Classify"})),
                &session,
                |_| async { panic!("Mod answers tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"]["label"], "bug");
        assert_eq!(result["result"]["noMatchIsUndefined"], true);
        assert_eq!(result["result"]["exactLabelPreserved"], true);
    }

    struct PendingModelSession {
        cwd: PathBuf,
        started: std::sync::Arc<std::sync::atomic::AtomicBool>,
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    struct MarkModelCallDropped(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for MarkModelCallDropped {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl ModSessionContext for PendingModelSession {
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }

        fn root(&self) -> PathBuf {
            self.cwd.clone()
        }

        async fn model(&self) -> String {
            "test".into()
        }

        async fn id(&self) -> String {
            "pending-model".into()
        }

        async fn turns(&self) -> u64 {
            1
        }

        async fn model_complete(
            &self,
            _input: Utf16JsonProjection,
            _plugin: &str,
        ) -> Result<Utf16JsonProjection, ModError> {
            let _dropped = MarkModelCallDropped(self.cancelled.clone());
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn model_complete_signal_aborts_inflight_host_call() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("cancel-complete.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', async ($) => {
                const controller = new AbortController();
                void $.clock.sleep(100).then(() => controller.abort());
                return { result: await $.model.complete(
                  { model: 'claude-sonnet-4-6', prompt: 'question' },
                  { signal: controller.signal }
                ) };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "cancel-complete",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let session = PendingModelSession {
            cwd: dir.path().to_path_buf(),
            started: started.clone(),
            cancelled: cancelled.clone(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            host.dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Complete"})),
                &session,
                |_| async { panic!("Mod answers tool.call") },
                |_, _| async {},
            ),
        )
        .await
        .expect("aborted completion must settle promptly")
        .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"]["reason"], "aborted");
        assert_eq!(result["result"]["usage"]["input_tokens"], 0);
        assert!(started.load(std::sync::atomic::Ordering::SeqCst));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the host request future must be dropped on abort");
    }

    #[tokio::test]
    async fn state_get_set_versions_owner_and_hook_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("state.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              const previous = [];
              on('state.set', ($, e, next) => {
                previous.push(Object.hasOwn(e, 'previous') ? e.previous : 'missing');
                return next({ ...e, value: e.value + 1 });
              });
              on('tool.call', async ($) => {
                const ref = { plugin: 'state-test', key: 'counter' };
                const missing = await $.state.get(ref);
                const first = await $.state.set(ref, 1);
                const current = await $.state.get(ref);
                const stale = await $.state.set(ref, 9, { ifVersion: 0 });
                const next = await $.state.set(ref, 4, { ifVersion: 1 });
                const after = await $.state.get(ref);
                const emptyId = await $.state.set({ ...ref, id: '' }, 7);
                const emptyIdValue = await $.state.get({ ...ref, id: '' });
                let denied;
                try { await $.state.set({ plugin: 'other', key: 'counter' }, 1); }
                catch (error) { denied = String(error.message); }
                return { result: {
                  missingVersion: missing.version,
                  missingIsUndefined: missing.value === undefined,
                  first, current, stale, next, after, emptyId, emptyIdValue,
                  denied, previous,
                } };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "state-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"State"})),
                |_| async { panic!("Mod answers tool.call") },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        let result = &result["result"];
        assert_eq!(result["missingVersion"], 0);
        assert_eq!(result["missingIsUndefined"], true);
        assert_eq!(result["first"], json!({"isSet":true,"version":1}));
        assert_eq!(result["current"], json!({"value":2,"version":1}));
        assert_eq!(result["stale"], json!({"isSet":false,"version":1}));
        assert_eq!(result["next"], json!({"isSet":true,"version":2}));
        assert_eq!(result["after"], json!({"value":5,"version":2}));
        assert_eq!(result["emptyId"], json!({"isSet":true,"version":1}));
        assert_eq!(result["emptyIdValue"], json!({"value":8,"version":1}));
        assert_eq!(result["previous"], json!(["missing", 2, 2, "missing"]));
        assert!(result["denied"]
            .as_str()
            .is_some_and(|reason| reason.contains("only its owner writes it")));
    }

    #[tokio::test]
    async fn state_survives_plugin_reload_in_same_session() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("state-reload.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', async ($) => {
                const ref = { plugin: 'state-reload', key: 'saved' };
                const before = await $.state.get(ref);
                if (before.version === 0) await $.state.set(ref, { count: 1 });
                return { result: before };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "state-reload",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let first = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"State"})),
                |_| async { panic!("Mod answers tool.call") },
            )
            .await
            .unwrap();
        let first = mod_test_value(&first);
        assert_eq!(first["result"], json!({"version":0}));
        host.unload("state-reload").await.unwrap();
        host.load(
            "state-reload",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let second = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"State"})),
                |_| async { panic!("Mod answers tool.call") },
            )
            .await
            .unwrap();
        let second = mod_test_value(&second);
        assert_eq!(second["result"], json!({"value":{"count":1},"version":1}));
    }

    struct TimerOutputSession {
        cwd: PathBuf,
        statuses: std::sync::Mutex<Vec<(String, Option<Utf16JsonProjection>)>>,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for TimerOutputSession {
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }

        fn root(&self) -> PathBuf {
            self.cwd.clone()
        }

        async fn model(&self) -> String {
            "test".into()
        }

        async fn id(&self) -> String {
            "timer-session".into()
        }

        async fn turns(&self) -> u64 {
            1
        }

        async fn emit_mod_status(&self, plugin: &str, text: Option<Utf16JsonProjection>) {
            self.statuses.lock().unwrap().push((plugin.into(), text));
        }
    }

    #[tokio::test]
    async fn explicitly_selected_bun_executable_runs_worker() {
        let Some(executable) = std::env::var_os(MOD_BUN_EXECUTABLE_ENV) else {
            return;
        };
        let executable = Path::new(&executable);
        let host = ModHost::start(Some(executable)).await.unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":"core"}));
    }

    #[tokio::test]
    async fn prepared_module_evaluates_the_scanned_source_after_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
          on('tool.call', () => ({ result: 'scanned' }));
        }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        let prepared = host.prepare_module(dir.path(), &module).await.unwrap();
        assert_eq!(prepared.uses["events"], json!(["tool.call"]));
        std::fs::write(
            &module,
            r#"export function register(on) {
          on('tool.call', () => ({ result: 'changed' }));
        }"#,
        )
        .unwrap();
        host.load_with_tier_order_storage_prepared(
            "candidate",
            "candidate",
            dir.path(),
            &module,
            test_mod_options(json!({})),
            "user",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"scanned"}));
    }

    #[tokio::test]
    async fn mod_rewrites_wraps_and_keeps_state() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(
            &module,
            r#"
          let calls = 0;
          export function register(on) {
            on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
              calls++;
              const result = await next({ ...e, command: e.command + ' --checked' });
              return { ...result, count: calls };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("test", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        for expected in 1..=2 {
            let result = host
                .dispatch(
                    "tool.call",
                    test_projection(json!({"tool":"Bash","command":"ls"})),
                    |e| async move {
                        let e = mod_test_event(e.clone());
                        assert_eq!(e["command"], "ls --checked");
                        Ok(Some(test_projection(json!({"result":"ok"}))))
                    },
                )
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result, json!({"result":"ok","count":expected}));
        }
    }

    #[tokio::test]
    async fn mod_loads_typescript_and_relative_import() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("helper.js"),
            "export const suffix = '-ts';\n",
        )
        .unwrap();
        let module = dir.path().join("register.ts");
        std::fs::write(
            &module,
            r#"
          import { suffix } from './helper.js';
          export function register(on: (event: string, handler: Function) => void) {
            on('tool.call', async ($: unknown, e: { command: string }, next: Function) =>
              next({ ...e, command: e.command + suffix }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("ts-test", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"command":"hi"})),
                |e| async move { Ok(e) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"command":"hi-ts"}));
    }

    #[tokio::test]
    async fn agent_spawn_mod_rewrites_content_and_preserves_pinned_identity() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("spawn.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('agent.spawn', { fork: true }, async ($, e, next) => {
                const result = await next({ ...e, prompt: e.prompt + ' inherited', model: 'haiku' });
                return { ...result, model: result.model + '-seen' };
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load_with_tier(
            "agents-md",
            dir.path(),
            &module,
            test_mod_options(json!({})),
            "builtin",
        )
        .await
        .unwrap();
        let input = json!({
            "tool_use_id":"tool-1", "prompt":"Read", "description":"Read docs",
            "subagentType":"fork", "provider":{"plugin":"engine","tier":"core"},
            "parentModel":"claude-sonnet", "parentAgentId":"parent-1",
            "permissionMode":"default", "background":false, "fork":true,
            "name":"reader"
        });
        let result = host
            .dispatch("agent.spawn", test_projection(input.clone()), |event| {
                let event = mod_test_event(event.clone());
                let input = input.clone();
                async move {
                    assert_eq!(event["prompt"], "Read inherited");
                    assert_eq!(event["model"], "haiku");
                    for key in [
                        "tool_use_id",
                        "provider",
                        "parentModel",
                        "parentAgentId",
                        "fork",
                        "name",
                    ] {
                        assert_eq!(event[key], input[key], "{key} was changed");
                    }
                    Ok(Some(test_projection(
                        json!({"model":"claude-haiku", "agentId":"child-1"}),
                    )))
                }
            })
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result,
            json!({"model":"claude-haiku-seen", "agentId":"child-1"})
        );
    }

    #[tokio::test]
    async fn agent_spawn_mod_cannot_forward_another_parent_or_agent_provider() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("spawn.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('agent.spawn', async ($, e, next) =>
                next({ ...e, parentAgentId: 'other', provider: { plugin: 'other', tier: 'user' } }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("pin-test", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let input = json!({
            "tool_use_id":"tool-1", "prompt":"Read", "description":"Read docs",
            "subagentType":"Explore", "provider":{"plugin":"engine","tier":"core"},
            "parentModel":"claude-sonnet", "parentAgentId":"parent-1",
            "background":false, "fork":false
        });
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let result = host
            .dispatch("agent.spawn", test_projection(input), move |_| {
                let count = count.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Some(test_projection(
                        json!({"model":"claude-sonnet", "agentId":"child-1"}),
                    )))
                }
            })
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["agentId"], "child-1");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn agent_spawn_restores_only_the_two_native_optional_identity_fields() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("spawn.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
            on('agent.spawn', ($, e, next) => {
              const { parentAgentId, provider, ...withoutTwo } = e;
              return next(withoutTwo);
            });
        }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("restore", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let input = json!({
            "tool_use_id":"tool-1", "prompt":"Read", "description":"Read docs",
            "subagentType":"Explore", "provider":{"plugin":"engine","tier":"core"},
            "parentModel":"claude-sonnet", "parentAgentId":"parent-1",
            "background":false, "fork":false
        });
        let expected = input.clone();
        let result = host
            .dispatch("agent.spawn", test_projection(input), move |event| {
                let event = mod_test_event(event.clone());
                let expected = expected.clone();
                async move {
                    assert_eq!(event, expected);
                    Ok(Some(test_projection(
                        json!({"model":"claude-sonnet", "agentId":"child-1"}),
                    )))
                }
            })
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["agentId"], "child-1");
    }

    #[tokio::test]
    async fn agent_spawn_rejects_relative_cwd_and_empty_rewritten_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("spawn.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
            on('agent.spawn', ($, e, next) => next(e.prompt === 'cwd'
              ? { ...e, cwd: 'relative/path' }
              : { ...e, prompt: '   ' }));
        }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("validate", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        for prompt in ["cwd", "empty"] {
            let result = host
                .dispatch(
                    "agent.spawn",
                    test_projection(json!({
                        "tool_use_id":"tool-1", "prompt":prompt, "description":"Read docs",
                        "subagentType":"Explore", "provider":{"plugin":"engine","tier":"core"},
                        "parentModel":"claude-sonnet", "background":false, "fork":false
                    })),
                    move |event| async move {
                        let event = mod_test_event(event.clone());
                        assert_eq!(event["prompt"], prompt);
                        assert!(event.get("cwd").is_none());
                        Ok(Some(test_projection(json!({"model":"claude-sonnet"}))))
                    },
                )
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result["model"], "claude-sonnet");
        }
    }

    /// Run with `CLAUDE_AGENTS_MD_SOURCE=/path/to/official/mods/agents-md`
    /// when validating an upstream release. The source is intentionally not
    /// vendored into LingXi; the synthetic tests above run on every build.
    #[tokio::test]
    async fn upstream_agents_md_module_loads_when_source_is_supplied() {
        struct AgentsMdSession {
            root: PathBuf,
        }

        #[async_trait::async_trait]
        impl ModSessionContext for AgentsMdSession {
            fn cwd(&self) -> PathBuf {
                self.root.clone()
            }

            fn root(&self) -> PathBuf {
                self.root.clone()
            }

            async fn model(&self) -> String {
                "claude-sonnet".into()
            }

            async fn id(&self) -> String {
                "agents-md-test".into()
            }

            async fn turns(&self) -> u64 {
                0
            }

            async fn fs_ancestors(
                &self,
                input: Utf16JsonProjection,
            ) -> Result<Utf16JsonProjection, ModError> {
                let input = mod_test_value(&input);
                let names = input["names"].as_array().cloned().unwrap_or_default();
                if !names.iter().any(|name| name == "AGENTS.md") {
                    return Ok(test_projection(json!([])));
                }
                let directory = if input.get("of").is_some() {
                    self.root.join("nested")
                } else {
                    self.root.clone()
                };
                let path = directory.join("AGENTS.md");
                let content = std::fs::read_to_string(&path).unwrap();
                Ok(test_projection(json!([{
                    "dir": directory, "name": "AGENTS.md", "content": content,
                    "parts": [{"path": path, "content": content}]
                }])))
            }
        }

        let Some(root) = std::env::var_os("CLAUDE_AGENTS_MD_SOURCE").map(PathBuf::from) else {
            return;
        };
        let module = root.join("hooks/register.ts");
        let host = ModHost::start(None).await.unwrap();
        let prepared = host.prepare_module(&root, &module).await.unwrap();
        let events = prepared.uses["events"].as_array().unwrap();
        assert!(events.contains(&json!("agent.spawn")));
        host.load_with_tier_order_storage_prepared(
            "agents-md",
            "agents-md@builtin",
            &root,
            &module,
            test_mod_options(json!({})),
            "builtin",
            None,
            Some(&prepared),
        )
        .await
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("nested")).unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "Root rule\n").unwrap();
        std::fs::write(project.path().join("nested/AGENTS.md"), "Nested rule\n").unwrap();
        let session = AgentsMdSession {
            root: project.path().to_path_buf(),
        };
        let context = host
            .dispatch_with_log_at_session(
                "prompt.context",
                test_projection(json!({"blocks": [], "instructionFiles": []})),
                &session,
                |event| async move { Ok(event) },
                |_, _| async {},
            )
            .await
            .unwrap();
        let context = mod_test_value(&context);
        assert_eq!(
            context["instructionFiles"][0]["path"],
            json!(project.path().join("AGENTS.md"))
        );
        let admission = host
            .begin_agent_spawn(
                test_projection(json!({
                    "tool_use_id":"tool-1", "prompt":"Read", "description":"Read docs",
                    "subagentType":"fork", "provider":{"plugin":"engine","tier":"core"},
                    "parentModel":"claude-sonnet", "background":false, "fork":true
                })),
                lingxi_core::host::subagent_spawn::AgentSpawnProvenance::default(),
            )
            .await
            .unwrap();
        let ModAgentSpawnAdmission::Forwarded { input, start } = admission else {
            panic!("official agents-md must forward the fork spawn");
        };
        assert_eq!(mod_test_value(&input)["subagentType"], "fork");
        start.started("child-1".into(), "claude-sonnet".into());

        let read = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({
                    "tool": "Read", "tool_use_id": "tool-2", "agentId": "child-1",
                    "file_path": project.path().join("nested/file.txt")
                })),
                &session,
                |_| async {
                    Ok(Some(test_projection(
                        json!({"result":"file data","text":"file data","ref":0}),
                    )))
                },
                |_, _| async {},
            )
            .await
            .unwrap();
        let read = mod_test_value(&read);
        assert_eq!(
            read["context"],
            json!([format!(
                "Contents of {}:\n\nNested rule\n",
                project.path().join("nested/AGENTS.md").display()
            )])
        );
        let second = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({
                    "tool": "Read", "tool_use_id": "tool-3", "agentId": "child-1",
                    "file_path": project.path().join("nested/file.txt")
                })),
                &session,
                |_| async {
                    Ok(Some(test_projection(
                        json!({"result":"file data","text":"file data","ref":0}),
                    )))
                },
                |_, _| async {},
            )
            .await
            .unwrap();
        let second = mod_test_value(&second);
        assert!(second.get("context").is_none());
    }

    #[tokio::test]
    async fn agent_spawn_next_arrives_before_the_started_result() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("spawn.js");
        std::fs::write(
            &module,
            r#"
            let inherited;
            export function register(on) {
              on('agent.spawn', async ($, e, next) => {
                const result = await next({ ...e, prompt: e.prompt + ' inherited' });
                inherited = result.agentId;
                return result;
              });
              on('tool.call', () => ({ result: { inherited } }));
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("inherit", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let admission = tokio::time::timeout(
            Duration::from_secs(2),
            host.begin_agent_spawn(
                test_projection(json!({
                    "tool_use_id":"tool-1", "prompt":"Read", "description":"Read docs",
                    "subagentType":"fork", "provider":{"plugin":"engine","tier":"core"},
                    "parentModel":"claude-sonnet", "background":false, "fork":true
                })),
                lingxi_core::host::subagent_spawn::AgentSpawnProvenance::default(),
            ),
        )
        .await
        .expect("next(e) must reach core before the agent starts")
        .unwrap();
        let ModAgentSpawnAdmission::Forwarded { input, start } = admission else {
            panic!("the hook passed the spawn on");
        };
        assert_eq!(mod_test_value(&input)["prompt"], "Read inherited");
        let before = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async { Ok(Some(test_projection(json!({"result":{"fallback":true}})))) },
            )
            .await
            .unwrap();
        let before = mod_test_value(&before);
        assert_eq!(before, json!({"result":{}}));
        start.started("agent:child-1".into(), "claude-sonnet".into());
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let after = host
                    .dispatch(
                        "tool.call",
                        test_projection(json!({"tool":"Read"})),
                        |_| async {
                            Ok(Some(test_projection(json!({"result":{"fallback":true}}))))
                        },
                    )
                    .await
                    .unwrap();
                let after = mod_test_value(&after);
                if after["result"]["inherited"] == "agent:child-1" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the hook receives the real child id after startup");
    }

    #[tokio::test]
    async fn agent_spawn_answer_without_next_starts_no_child() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("deny.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
            on('agent.spawn', () => ({ deny: 'policy refused the child' }));
        }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("deny", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .begin_agent_spawn(
                test_projection(json!({
                    "tool_use_id":"tool-1", "prompt":"Read", "description":"Read docs",
                    "subagentType":"Explore", "provider":{"plugin":"engine","tier":"core"},
                    "parentModel":"claude-sonnet", "background":false, "fork":false
                })),
                lingxi_core::host::subagent_spawn::AgentSpawnProvenance::default(),
            )
            .await
            .unwrap();
        let ModAgentSpawnAdmission::Answered(answer) = result else {
            panic!("a denial must not reach core");
        };
        assert_eq!(
            mod_test_value(&answer),
            json!({"deny":"policy refused the child"})
        );
    }

    #[tokio::test]
    async fn mod_resolves_extensionless_typescript_and_index_imports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(
            dir.path().join("helper.ts"),
            "import type { On } from 'claude-code';\nexport const suffix: string = '-ts';\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("nested/index.ts"),
            "export async function home($: any): Promise<string | undefined> { return $.env.get('HOME'); }\n",
        )
        .unwrap();
        let module = dir.path().join("register.ts");
        std::fs::write(
            &module,
            r#"
            import { suffix } from './helper.js'
            import { home } from './nested'
            export function register(on: (event: string, handler: Function) => void) {
              on('tool.call', async ($: any) => ({ result: (await home($)) + suffix }));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "extensionless-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async { panic!("Mod answers tool.call") },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        let home = std::env::var_os("HOME")
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "undefined".into());
        assert_eq!(result["result"], format!("{home}-ts"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn extensionless_import_cannot_follow_a_symlink_outside_the_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("secret.ts"),
            "export const secret = 'outside';",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.ts"), dir.path().join("linked"))
            .unwrap();
        let module = dir.path().join("register.ts");
        std::fs::write(
            &module,
            "import { secret } from './linked'; export function register(on) { on('tool.call', () => ({ result: secret })); }",
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        let error = host
            .load(
                "escape-test",
                dir.path(),
                &module,
                test_mod_options(json!({})),
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("resolved source leaves the plugin root"));
    }

    #[tokio::test]
    async fn mod_links_shared_import_graph_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("common.ts"),
            "let count = 0; export function nextCount() { return ++count; }",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("left.ts"),
            "import { nextCount } from './common.js'; export const left = () => nextCount();",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("right.ts"),
            "import { nextCount } from './common.js'; export const right = () => nextCount();",
        )
        .unwrap();
        let module = dir.path().join("register.ts");
        std::fs::write(
            &module,
            "import { left } from './left'; import { right } from './right'; export function register(on) { on('tool.call', () => ({ result: [left(), right()] })); }",
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "shared-import-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async { panic!("Mod answers tool.call") },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"], json!([1, 2]));
    }

    #[tokio::test]
    async fn mods_wrap_in_load_order_and_unload_by_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.js");
        let second = dir.path().join("second.js");
        std::fs::write(
            &first,
            r#"
          export function register(on) {
            on('tool.call', async ($, e, next) => {
              const result = await next({ ...e, command: e.command + 'A' });
              return { ...result, text: result.text + 'a' };
            });
          }
        "#,
        )
        .unwrap();
        std::fs::write(
            &second,
            r#"
          export function register(on) {
            on('tool.call', { command: 'xA' }, async ($, e, next) => {
              const result = await next({ ...e, command: e.command + 'B' });
              return { ...result, text: result.text + 'b' };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("first", dir.path(), &first, test_mod_options(json!({})))
            .await
            .unwrap();
        host.load("second", dir.path(), &second, test_mod_options(json!({})))
            .await
            .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash","command":"x"})),
                |e| async move {
                    let e = mod_test_event(e.clone());
                    Ok(Some(test_projection(
                        json!({"result":e["command"],"text":e["command"]}),
                    )))
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["text"], "xABba");
        host.unload("first").await.unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash","command":"x"})),
                |e| async move {
                    let e = mod_test_event(e.clone());
                    Ok(Some(test_projection(
                        json!({"result":e["command"],"text":e["command"]}),
                    )))
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["text"], "x");
    }

    #[tokio::test]
    async fn unsupported_event_rejects_module_without_partial_registration() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', () => ({ deny: 'should not remain' }));
            on('ui.nonexistent', () => ({ type: 'Text', props: {}, children: [] }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        assert!(host
            .load("partial", dir.path(), &module, test_mod_options(json!({})))
            .await
            .is_err());
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
    }

    #[tokio::test]
    async fn tool_call_rejects_empty_context_and_keeps_the_core_answer() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('tool.call', () => ({ result: 'fake', context: [''] }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "context-shape",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
    }

    #[tokio::test]
    async fn tool_call_cannot_drop_a_downstream_context_entry() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('tool.call', async ($, e, next) => {
                const answer = await next(e);
                return { ...answer, context: ['first'] };
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "context-preservation",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async {
                    Ok(Some(test_projection(
                        json!({"result":"core","context":["first","first"]}),
                    )))
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core","context":["first","first"]}));
    }

    #[tokio::test]
    async fn tool_call_requires_result_and_compares_context_result_keys_canonically() {
        let dir = tempfile::tempdir().unwrap();
        let invalid = dir.path().join("invalid.js");
        std::fs::write(
            &invalid,
            r#"export function register(on) {
              on('tool.call', () => ({ text: 'missing result' }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "invalid-shape",
            dir.path(),
            &invalid,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
        host.unload("invalid-shape").await.unwrap();

        let valid = dir.path().join("valid.js");
        std::fs::write(
            &valid,
            r#"export function register(on) {
              on('tool.call', async ($, e, next) => {
                await next(e);
                return { result: { b: 2, a: 1 }, context: ['from core', 'extra'] };
              });
            }"#,
        )
        .unwrap();
        host.load(
            "canonical-context",
            dir.path(),
            &valid,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async {
                    Ok(Some(test_projection(
                        json!({"result":{"a":1,"b":2},"context":["from core"]}),
                    )))
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["result"], json!({"a":1,"b":2}));
        assert_eq!(answer["context"], json!(["from core", "extra"]));
    }

    #[tokio::test]
    async fn mod_tiers_nest_in_authority_order_not_load_order() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for tier in ["builtin", "append", "user", "prepend"] {
            let module = dir.path().join(format!("{tier}.js"));
            std::fs::write(
                &module,
                r#"
              export function register(on, options) {
                on('tool.call', async ($, e, next) => {
                  const result = await next(e);
                  return { result: { order: [options.tier, ...result.result.order] } };
                });
              }
            "#,
            )
            .unwrap();
            host.load_with_tier(
                tier,
                dir.path(),
                &module,
                test_mod_options(json!({"tier":tier})),
                tier,
            )
            .await
            .unwrap();
        }
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":{"order":["core"]}})))) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"]["order"],
            json!(["prepend", "user", "append", "builtin", "core"])
        );
    }

    #[tokio::test]
    async fn managed_tier_uses_explicit_policy_order() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for (name, order) in [("second", 1), ("first", 0)] {
            let module = dir.path().join(format!("{name}.js"));
            std::fs::write(
                &module,
                r#"
              export function register(on, options) {
                on('tool.call', async ($, e, next) => {
                  const result = await next(e);
                  return { result: { order: [options.name, ...result.result.order] } };
                });
              }
            "#,
            )
            .unwrap();
            host.load_with_tier_order(
                name,
                dir.path(),
                &module,
                test_mod_options(json!({"name":name})),
                "prepend",
                Some(order),
            )
            .await
            .unwrap();
        }
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":{"order":["core"]}})))) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"]["order"],
            json!(["first", "second", "core"])
        );
    }

    #[tokio::test]
    async fn sec_default_order_skips_user_tier_at_its_managed_seat() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for (name, tier, order, source) in [
            (
                "guard",
                "prepend",
                Some(0),
                r#"
                export function register(on) {
                  on('prompt.context', async ($, e, next) => {
                    const result = await next(e);
                    return { ...result, blocks: [...result.blocks,
                      { name: 'trace', text: next.trace.map(x =>
                        `${x.plugin}:${x.outcome}:${x.reason ?? ''}`).join('|') }] };
                  });
                }
            "#,
            ),
            (
                "user",
                "user",
                None,
                r#"
                export function register(on) {
                  on('prompt.context', ($, e, next) => next({ ...e,
                    blocks: e.blocks.filter(block => block.name !== 'policy') }));
                }
            "#,
            ),
            (
                "audit",
                "append",
                None,
                r#"
                export function register(on) {
                  on('prompt.context', async ($, e, next) => {
                    const result = await next(e);
                    return { ...result, blocks: [...result.blocks,
                      { name: 'audit', text: 'seen' }] };
                  });
                }
            "#,
            ),
        ] {
            let module = dir.path().join(format!("{name}.js"));
            std::fs::write(&module, source).unwrap();
            host.load_with_tier_order(
                name,
                dir.path(),
                &module,
                test_mod_options(json!({})),
                tier,
                order,
            )
            .await
            .unwrap();
        }
        let input = json!({"blocks":[{"name":"policy","text":"managed"}],"instructionFiles":[]});
        host.set_sec_default_order(Some(-1));
        let implicit = host
            .dispatch(
                "prompt.context",
                test_projection(input.clone()),
                |e| async move { Ok(e) },
            )
            .await
            .unwrap();
        let implicit = mod_test_value(&implicit);
        assert_eq!(implicit["blocks"][0], input["blocks"][0]);
        assert_eq!(implicit["blocks"][1]["name"], "audit");
        assert_eq!(implicit["blocks"].as_array().unwrap().len(), 2);

        host.set_sec_default_order(Some(1));
        let explicit = host
            .dispatch(
                "prompt.context",
                test_projection(input.clone()),
                |e| async move { Ok(e) },
            )
            .await
            .unwrap();
        let explicit = mod_test_value(&explicit);
        assert_eq!(explicit["blocks"][0], input["blocks"][0]);
        assert_eq!(explicit["blocks"][1]["name"], "audit");
        let trace = explicit["blocks"][2]["text"].as_str().unwrap();
        assert!(
            trace.contains("user:skipped:bypassed by cc-plugin-sec-default"),
            "{trace}"
        );

        host.set_sec_default_order(None);
        let unseated = host
            .dispatch("prompt.context", test_projection(input), |e| async move {
                Ok(e)
            })
            .await
            .unwrap();
        let unseated = mod_test_value(&unseated);
        assert!(unseated["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["name"] != "policy"));
    }

    #[tokio::test]
    async fn prompt_context_api_dispatches_and_returns_sections_directly() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("context-api.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('prompt.context', async ($, e, next) => {
                const detached = await $.prompt.context(e);
                if (detached.blocks.length !== e.blocks.length) {
                  throw new Error('calling frame was not skipped');
                }
                const answer = await next(e);
                return { ...answer, blocks: [...answer.blocks,
                  { name: 'hook', text: 'rewritten' }] };
              });
              on('tool.call', async ($) => {
                const malformed = '\uD800';
                const answer = await $.prompt.context({
                  blocks: [{ name: 'base', text: malformed, [malformed]: malformed }],
                  instructionFiles: [],
                });
                const keyRoundTrip = answer.blocks[0].text === malformed
                  && answer.blocks[0][malformed] === malformed
                  && Object.keys(answer.blocks[0]).includes(malformed);
                let invalid = false;
                try { await $.prompt.context({ blocks: [{ name: 'bad', text: 7 }] }); }
                catch { invalid = true; }
                return { result: { answerText: answer.blocks[0].text === malformed ? 'exact' : 'changed',
                  keyRoundTrip, invalid } };
              });
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "context-api",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { panic!("Mod handles tool.call") },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"]["answerText"], "exact");
        assert_eq!(result["result"]["keyRoundTrip"], true);
        assert_eq!(result["result"]["invalid"], true);
    }

    #[tokio::test]
    async fn prompt_context_result_accepts_repeated_names_for_native_from_entries() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("context-duplicate.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('prompt.context', ($, e, next) => next({
                ...e,
                blocks: [
                  { name: 'repeat', text: 'first' },
                  { name: 'after', text: 'middle' },
                  { name: 'repeat', text: 'last' },
                ],
              }));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "context-duplicate",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "prompt.context",
                test_projection(json!({
                    "blocks": [{"name":"base","text":"original"}],
                    "instructionFiles": [],
                })),
                |event| async move { Ok(event) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["blocks"],
            json!([
                {"name":"repeat","text":"first"},
                {"name":"after","text":"middle"},
                {"name":"repeat","text":"last"},
            ])
        );
    }

    #[tokio::test]
    async fn next_to_skips_only_tiers_allowed_to_the_managed_hook() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for tier in ["builtin", "user", "append", "prepend"] {
            let module = dir.path().join(format!("{tier}.js"));
            std::fs::write(
                &module,
                r#"
              export function register(on, options) {
                on('tool.call', async ($, e, next) => {
                  let result;
                  if (options.tier === 'prepend' && e.jump === 'builtin')
                    result = await next.to(e, 'builtin');
                  else if (options.tier === 'prepend' && e.jump === 'append')
                    result = await next.to(e, 'append');
                  else if (options.tier === 'append' && e.jump === 'append-core')
                    result = await next.to(e, 'core');
                  else result = await next(e);
                  return { result: { order: [options.tier, ...result.result.order] } };
                });
              }
            "#,
            )
            .unwrap();
            host.load_with_tier(
                tier,
                dir.path(),
                &module,
                test_mod_options(json!({"tier":tier})),
                tier,
            )
            .await
            .unwrap();
        }
        for (jump, expected) in [
            ("builtin", json!(["prepend", "builtin", "core"])),
            ("append", json!(["prepend", "append", "builtin", "core"])),
            ("append-core", json!(["prepend", "user", "append", "core"])),
        ] {
            let result = host
                .dispatch(
                    "tool.call",
                    test_projection(json!({"tool":"Bash","jump":jump})),
                    |_| async { Ok(Some(test_projection(json!({"result":{"order":["core"]}})))) },
                )
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result["result"]["order"], expected, "jump {jump}");
        }
    }

    #[tokio::test]
    async fn invalid_user_next_to_fails_open_without_skipping_core() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("user.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', ($, e, next) => next.to(e, 'core'));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("user", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
    }

    #[tokio::test]
    async fn next_trace_records_downstream_outcomes_and_latest_call() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for (name, source) in [
            (
                "outer",
                r#"
                export function register(on) {
                  on('tool.call', async ($, e, next) => {
                    const before = next.trace.length;
                    await next({ ...e, run: 1 });
                    const first = next.trace.map(x => x.received.run);
                    const answer = await next({ ...e, run: 2 });
                    const trace = next.trace.map(x => ({ index: x.index, plugin: x.plugin,
                      tier: x.tier, event: x.event, outcome: x.outcome,
                      received: x.received.run, returned: x.returned?.result?.run,
                      frozen: Object.isFrozen(x), ms: x.ms >= 0 }));
                    return { result: { before, first, trace, run: answer.result.run } };
                  });
                }
                "#,
            ),
            (
                "inner",
                r#"
                export function register(on) {
                  on('tool.call', async ($, e, next) => next(e));
                }
                "#,
            ),
        ] {
            let module = dir.path().join(format!("{name}.js"));
            std::fs::write(&module, source).unwrap();
            host.load(name, dir.path(), &module, test_mod_options(json!({})))
                .await
                .unwrap();
        }
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |e| async move {
                    let e = mod_test_event(e.clone());
                    Ok(Some(test_projection(json!({"result":{"run":e["run"]}}))))
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["result"]["before"], 0);
        assert_eq!(answer["result"]["first"], json!([1, 1]));
        assert_eq!(answer["result"]["run"], 2);
        assert_eq!(
            answer["result"]["trace"],
            json!([
                {"index":1,"plugin":"inner","tier":"user","event":"tool.call","outcome":"passed","received":2,"returned":2,"frozen":true,"ms":true},
                {"index":2,"plugin":"engine","tier":"core","event":"tool.call","outcome":"returned","received":2,"returned":2,"frozen":true,"ms":true}
            ])
        );
    }

    #[tokio::test]
    async fn next_trace_names_bypassed_tiers_and_failing_hooks_yield_downstream() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for (name, tier, source) in [
            (
                "managed",
                "prepend",
                r#"
                export function register(on) {
                  on('tool.call', async ($, e, next) => {
                    const answer = await next.to(e, 'builtin');
                    return { ...answer, trace: next.trace.map(x => ({ plugin: x.plugin,
                      outcome: x.outcome, reason: x.reason ?? null,
                      returned: x.returned?.result ?? null })) };
                  });
                }
                "#,
            ),
            (
                "user",
                "user",
                r#"export function register(on) { on('tool.call', () => { throw Error('must be bypassed'); }); }"#,
            ),
            (
                "append",
                "append",
                r#"export function register(on) { on('tool.call', () => { throw Error('must be bypassed'); }); }"#,
            ),
            (
                "builtin",
                "builtin",
                r#"export function register(on) { on('tool.call', async ($, e, next) => next(e)); }"#,
            ),
        ] {
            let module = dir.path().join(format!("{name}.js"));
            std::fs::write(&module, source).unwrap();
            host.load_with_tier(name, dir.path(), &module, test_mod_options(json!({})), tier)
                .await
                .unwrap();
        }
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(
            answer["trace"],
            json!([
                {"plugin":"user","outcome":"skipped","reason":"bypassed by managed","returned":null},
                {"plugin":"append","outcome":"skipped","reason":"bypassed by managed","returned":null},
                {"plugin":"builtin","outcome":"passed","reason":null,"returned":"core"},
                {"plugin":"engine","outcome":"returned","reason":null,"returned":"core"}
            ])
        );
    }

    #[tokio::test]
    async fn failed_hook_before_next_is_skipped_and_after_next_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for (name, source) in [
            (
                "observer",
                r#"
                export function register(on) {
                  on('tool.call', async ($, e, next) => {
                    const answer = await next(e);
                    return { ...answer, trace: next.trace.map(x => [x.plugin, x.outcome]) };
                  });
                }
                "#,
            ),
            (
                "before",
                r#"export function register(on) { on('tool.call', () => { throw Error('before'); }); }"#,
            ),
            (
                "after",
                r#"
                export function register(on) {
                  on('tool.call', async ($, e, next) => { await next(e); throw Error('after'); });
                }
                "#,
            ),
        ] {
            let module = dir.path().join(format!("{name}.js"));
            std::fs::write(&module, source).unwrap();
            host.load(name, dir.path(), &module, test_mod_options(json!({})))
                .await
                .unwrap();
        }
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["result"], "core");
        assert_eq!(
            answer["trace"],
            json!([
                ["before", "skipped"],
                ["after", "kept"],
                ["engine", "returned"]
            ])
        );
    }

    #[tokio::test]
    async fn tool_matcher_handles_one_of_and_regular_expression() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(&module, r#"
          export function register(on) {
            on('tool.call', { tool: ['Bash', 'Read'], command: /^git / }, () => ({ deny: 'matched' }));
          }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("matcher", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let blocked = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash","command":"git status"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let blocked = mod_test_value(&blocked);
        assert_eq!(blocked, json!({"deny":"matched"}));
        let allowed = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash","command":"ls"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let allowed = mod_test_value(&allowed);
        assert_eq!(allowed, json!({"result":"core"}));
    }

    #[tokio::test]
    async fn wildcard_tool_hook_runs_and_duplicate_registration_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let wildcard = dir.path().join("wildcard.js");
        std::fs::write(
            &wildcard,
            r#"
          export function register(on) {
            on('*', { tool: 'Bash' }, ($, e, next) => {
              if (next.event !== 'tool.call') throw new Error('wrong event');
              return next({ ...e, command: 'rewritten' });
            });
          }
        "#,
        )
        .unwrap();
        let duplicate = dir.path().join("duplicate.js");
        std::fs::write(
            &duplicate,
            r#"
          export function register(on) {
            on('tool.call', () => ({ deny: 'first' }));
            on('tool.call', () => ({ deny: 'second' }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "wildcard",
            dir.path(),
            &wildcard,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        assert!(host
            .load(
                "duplicate",
                dir.path(),
                &duplicate,
                test_mod_options(json!({}))
            )
            .await
            .is_err());
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash","command":"original"})),
                |e| async move { Ok(e) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["command"], "rewritten");
    }

    #[tokio::test]
    async fn next_is_selects_event_names_globs_and_negations() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("is.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('*', ($, e, next) => {
              const result = {
                event: next.event,
                all: next.is('*', e),
                tool: next.is('tool.call', e),
                fs: next.is('fs.*', e),
                notTool: next.is('!tool.call', e),
              };
              return next.event === 'tool.call' ? { result } : { value: JSON.stringify(result) };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("is-test", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let tool = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { panic!("core must not run") },
            )
            .await
            .unwrap();
        let tool = mod_test_value(&tool);
        assert_eq!(
            tool["result"],
            json!({"event":"tool.call","all":true,"tool":true,"fs":false,"notTool":false})
        );
        let file = host
            .dispatch(
                "fs.read",
                test_projection(json!({"path":"README.md"})),
                |_| async { panic!("core must not run") },
            )
            .await
            .unwrap();
        let file = mod_test_value(&file);
        assert_eq!(
            serde_json::from_str::<Value>(file["value"].as_str().unwrap()).unwrap(),
            json!({"event":"fs.read","all":true,"tool":false,"fs":true,"notTool":true})
        );
    }

    #[tokio::test]
    async fn event_glob_and_negation_wrap_only_selected_events() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("patterns.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('fs.*', async ($, e, next) => {
              const result = await next(e);
              return { value: `fs:${result.value}` };
            });
            on('!tool.call', async ($, e, next) => {
              const result = await next(e);
              return { value: `notTool:${result.value}` };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("patterns", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let file = host
            .dispatch(
                "fs.read",
                test_projection(json!({"path":"README.md"})),
                |_| async { Ok(Some(test_projection(json!({"value":"core"})))) },
            )
            .await
            .unwrap();
        let file = mod_test_value(&file);
        assert_eq!(file["value"], "fs:notTool:core");
        let tool = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"chain":["core"]})))) },
            )
            .await
            .unwrap();
        let tool = mod_test_value(&tool);
        assert_eq!(tool["chain"], json!(["core"]));
    }

    #[tokio::test]
    async fn catch_handler_receives_failed_next_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("catch.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', async ($, e, next) => next(e))
              .catch(($, e, next) => ({ deny: `${next.called}:${next.error.kind}`,
                budget: next.error.budget,
                trace: next.trace.map(x => [x.plugin, x.outcome]) }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "catch-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Err(ModError::Hook("core failed".into())) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(
            answer,
            json!({
                "deny":"true:throw", "budget":1000,
                "trace":[["engine","rejected"]]
            })
        );
    }

    #[tokio::test]
    async fn hook_budget_counts_own_time_and_sleep_but_pauses_for_next_and_other_api() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("budget.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('clock.now', async ($, e, next) => {
              const until = Date.now() + 80;
              while (Date.now() < until) {}
              return next(e);
            });
            on('tool.call', async ($, e, next) => {
              const initial = next.budget;
              await $.clock.now();
              const afterApi = next.budget;
              await next(e);
              const afterNext = next.budget;
              await $.clock.sleep(80);
              const afterSleep = next.budget;
              const until = Date.now() + 80;
              while (Date.now() < until) {}
              const afterOwn = next.budget;
              return { result: {
                ms: initial.ms,
                apiCost: initial.remainingMs - afterApi.remainingMs,
                nextCost: afterApi.remainingMs - afterNext.remainingMs,
                sleepCost: afterNext.remainingMs - afterSleep.remainingMs,
                ownCost: afterSleep.remainingMs - afterOwn.remainingMs,
              } };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("budget", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(Some(test_projection(json!({"result":"core"}))))
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["result"]["ms"], 10_000);
        assert!(answer["result"]["apiCost"].as_f64().unwrap() < 40.0);
        assert!(answer["result"]["nextCost"].as_f64().unwrap() < 40.0);
        assert!(answer["result"]["sleepCost"].as_f64().unwrap() >= 60.0);
        assert!(answer["result"]["ownCost"].as_f64().unwrap() >= 60.0);
    }

    #[tokio::test]
    async fn catch_replays_inflight_next_without_calling_core_twice() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("replay.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', ($, e, next) => {
              void next(e);
              throw Error('after next started');
            }).catch(async ($, e, next) => {
              const answer = await next(e);
              return { result: {
                answer: answer.result,
                called: next.called,
                remaining: next.budget.remainingMs,
                trace: next.trace.map(x => [x.plugin, x.outcome]),
              } };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("replay", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let core_calls = Arc::new(AtomicU64::new(0));
        let count = core_calls.clone();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                move |_| {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(120)).await;
                        Ok(Some(test_projection(json!({"result":"core"}))))
                    }
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(core_calls.load(Ordering::Relaxed), 1);
        assert_eq!(answer["result"]["answer"], "core");
        assert_eq!(answer["result"]["called"], true);
        assert!(answer["result"]["remaining"].as_f64().unwrap() > 900.0);
        assert_eq!(answer["result"]["trace"], json!([["engine", "returned"]]));
    }

    #[tokio::test]
    async fn expired_hook_enters_fresh_catch_budget_and_leaves_worker_usable() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("expired.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            let originalSignal;
            on('tool.call', async ($, e, next) => {
              originalSignal = next.signal;
              return new Promise(() => {});
            })
              .catch(($, e, next) => ({ result: {
                kind: next.error.kind,
                errorBudget: next.error.budget,
                catchBudget: next.budget.ms,
                remaining: next.budget.remainingMs,
                signalAborted: next.signal.aborted,
                originalSignalAborted: originalSignal.aborted,
              } }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("expired", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { panic!("a caught timeout must not call core") },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["result"]["kind"], "timeout");
        assert_eq!(answer["result"]["errorBudget"], 1_000);
        assert_eq!(answer["result"]["catchBudget"], 1_000);
        assert!(answer["result"]["remaining"].as_f64().unwrap() > 0.0);
        assert_eq!(answer["result"]["signalAborted"], false);
        assert_eq!(answer["result"]["originalSignalAborted"], true);
        let after = host
            .dispatch(
                "fs.exists",
                test_projection(json!({"path":"anything"})),
                |_| async { Ok(Some(test_projection(json!({"value":true})))) },
            )
            .await
            .unwrap();
        let after = mod_test_value(&after);
        assert_eq!(after, json!({"value":true}));
    }

    #[tokio::test]
    async fn uncaught_timeout_runs_core_and_traces_expired_link() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for (name, source) in [
            (
                "observer",
                r#"
                export function register(on) {
                  on('tool.call', async ($, e, next) => {
                    const answer = await next(e);
                    return { ...answer, trace: next.trace.map(x => [x.plugin, x.outcome]) };
                  });
                }
                "#,
            ),
            (
                "expired",
                r#"
                export function register(on) {
                  on('tool.call', async () => new Promise(() => {}));
                }
                "#,
            ),
        ] {
            let module = dir.path().join(format!("{name}.js"));
            std::fs::write(&module, source).unwrap();
            host.load(name, dir.path(), &module, test_mod_options(json!({})))
                .await
                .unwrap();
        }
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["result"], "core");
        assert_eq!(
            answer["trace"],
            json!([["expired", "expired"], ["engine", "returned"]])
        );
    }

    #[tokio::test]
    async fn synchronous_infinite_loop_expires_only_its_hook() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("spin.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', () => { while (true) {} });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("spin", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let answer = tokio::time::timeout(
            Duration::from_secs(16),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            ),
        )
        .await
        .expect("the hook timer must interrupt the loop before the worker watchdog")
        .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
        let after = host
            .dispatch(
                "fs.exists",
                test_projection(json!({"path":"anything"})),
                |_| async { Ok(Some(test_projection(json!({"value":true})))) },
            )
            .await
            .unwrap();
        let after = mod_test_value(&after);
        assert_eq!(after, json!({"value":true}));
    }

    #[tokio::test]
    async fn catch_grace_timeout_yields_to_core() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("grace.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', () => { throw Error('hook failed'); })
              .catch(async () => new Promise(() => {}));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("grace", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let start = std::time::Instant::now();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
        assert!(start.elapsed() >= Duration::from_millis(900));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn catch_grace_interrupts_synchronous_loop_without_killing_worker() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("grace-spin.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', () => { throw Error('hook failed'); })
              .catch(() => { while (true) {} });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "grace-spin",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let start = std::time::Instant::now();
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            ),
        )
        .await
        .expect("the catch VM timer must interrupt before the worker watchdog")
        .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
        assert!(start.elapsed() >= Duration::from_millis(900));
        let after = host
            .dispatch(
                "fs.exists",
                test_projection(json!({"path":"anything"})),
                |_| async { Ok(Some(test_projection(json!({"value":true})))) },
            )
            .await
            .unwrap();
        let after = mod_test_value(&after);
        assert_eq!(after, json!({"value":true}));
    }

    #[tokio::test]
    async fn next_without_event_rejects_before_core_runs() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("missing-next.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', async ($, e, next) => {
              try { await next(); }
              catch (error) { return { deny: String(error).includes('next() requires an event')
                ? 'missing event' : 'wrong error' }; }
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "missing-next",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { panic!("core must not run") },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"deny":"missing event"}));
    }

    #[tokio::test]
    async fn nested_dispatch_uses_the_same_worker_without_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("nested.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', async ($, e, next) => {
              $.ui.log(e.tool);
              const result = await next(e);
              return { ...result, trace: e.tool };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("nested", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let nested_host = host.clone();
        let outer_logs = Arc::new(Mutex::new(Vec::<String>::new()));
        let inner_logs = Arc::new(Mutex::new(Vec::<String>::new()));
        let outer_logs_for_callback = outer_logs.clone();
        let inner_logs_for_core = inner_logs.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            host.dispatch_with_log(
                "tool.call",
                test_projection(json!({"tool":"Outer"})),
                move |_| {
                    let nested_host = nested_host.clone();
                    let inner_logs = inner_logs_for_core.clone();
                    async move {
                        let nested = nested_host
                            .dispatch_with_log(
                                "tool.call",
                                test_projection(json!({"tool":"Inner"})),
                                |_| async { Ok(Some(test_projection(json!({"result":"inner"})))) },
                                move |_, line| {
                                    let inner_logs = inner_logs.clone();
                                    async move {
                                        inner_logs
                                            .lock()
                                            .await
                                            .push(line.string_at("").unwrap_or_default().to_owned())
                                    }
                                },
                            )
                            .await?;
                        let nested = mod_test_value(&nested);
                        Ok(Some(test_projection(
                            json!({"result":nested["result"],"nestedTrace":nested["trace"]}),
                        )))
                    }
                },
                move |_, line| {
                    let outer_logs = outer_logs_for_callback.clone();
                    async move {
                        outer_logs
                            .lock()
                            .await
                            .push(line.string_at("").unwrap_or_default().to_owned())
                    }
                },
            ),
        )
        .await
        .expect("nested dispatch must finish")
        .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result,
            json!({"result":"inner","nestedTrace":"Inner","trace":"Outer"})
        );
        assert_eq!(*outer_logs.lock().await, vec!["Outer"]);
        assert_eq!(*inner_logs.lock().await, vec!["Inner"]);
    }

    #[tokio::test]
    async fn ui_log_event_rewrites_sink_and_honors_denial() {
        let dir = tempfile::tempdir().unwrap();
        let policy = dir.path().join("policy.js");
        std::fs::write(
            &policy,
            r#"
          const seen = [];
          export function register(on) {
            on('ui.log', ($, e, next) => {
              seen.push({ text: e.text, to: e.to, origin: next.origin });
              if (e.text === 'drop') return { deny: 'blocked' };
              return next({ ...e, text: e.text.toUpperCase(),
                to: e.text === 'quiet' ? 'debug' : e.to });
            });
            on('tool.call', ($, e, next) =>
              e.tool === 'Query' ? { result: seen } : next(e));
          }
        "#,
        )
        .unwrap();
        let emitter = dir.path().join("emitter.js");
        std::fs::write(
            &emitter,
            r#"
          export function register(on) {
            on('tool.call', ($, e, next) => {
              if (e.tool !== 'Emit') return next(e);
              const returned = $.ui.log('visible');
              $.ui.log('quiet', { to: 'debug' });
              $.ui.log('drop');
              return { result: { returnedUndefined: returned === undefined } };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load_with_tier(
            "policy",
            dir.path(),
            &policy,
            test_mod_options(json!({})),
            "prepend",
        )
        .await
        .unwrap();
        host.load("emitter", dir.path(), &emitter, test_mod_options(json!({})))
            .await
            .unwrap();
        let logs = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let sink = logs.clone();
        let emitted = host
            .dispatch_with_log(
                "tool.call",
                test_projection(json!({"tool":"Emit"})),
                |_| async { panic!("the emitter answers this call") },
                move |plugin, line| {
                    let sink = sink.clone();
                    async move {
                        sink.lock()
                            .await
                            .push((plugin, line.string_at("").unwrap_or_default().to_owned()))
                    }
                },
            )
            .await
            .unwrap();
        let emitted = mod_test_value(&emitted);
        assert_eq!(emitted, json!({"result":{"returnedUndefined":true}}));
        assert_eq!(
            *logs.lock().await,
            vec![("emitter".into(), "VISIBLE".into())]
        );
        let seen = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Query"})),
                |_| async { panic!("policy answers the query") },
            )
            .await
            .unwrap();
        let seen = mod_test_value(&seen);
        assert_eq!(
            seen["result"],
            json!([
                {"text":"visible","to":"transcript","origin":{"plugin":"emitter","tier":"user"}},
                {"text":"quiet","to":"debug","origin":{"plugin":"emitter","tier":"user"}},
                {"text":"drop","to":"transcript","origin":{"plugin":"emitter","tier":"user"}}
            ])
        );
    }

    #[tokio::test]
    async fn ui_toast_rewrites_timeout_and_throttles_per_plugin_without_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("toast.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('ui.toast', ($, e, next) => next({ ...e, text: e.text.toUpperCase(), timeoutMs: 2500 }));
            on('tool.call', ($) => {
              const first = $.ui.toast('first');
              $.ui.toast('second');
              return { result: { returnedUndefined: first === undefined } };
            });
          }
        "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "session".into(),
            turns: 1,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load("toaster", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let toasts = Arc::new(Mutex::new(Vec::<(String, Utf16JsonProjection, u64)>::new()));
        let sink = toasts.clone();
        let result = host
            .dispatch_with_ui_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                &session,
                |_| async { panic!("Mod answers this call") },
                |_, _| async {},
                move |plugin, text, timeout_ms| {
                    let sink = sink.clone();
                    async move { sink.lock().await.push((plugin, text, timeout_ms)) }
                },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":{"returnedUndefined":true}}));
        let toasts = toasts.lock().await;
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].0, "toaster");
        assert_eq!(toasts[0].1.string_at(""), Some("FIRST"));
        assert_eq!(toasts[0].2, 2500);
    }

    #[tokio::test]
    async fn ui_toast_replaces_unpaired_surrogate_before_host_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("toast-unicode.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', ($) => {
              $.ui.toast(42);
              $.ui.toast('\uD800');
              return { result: 'done' };
            });
          }
        "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "session".into(),
            turns: 1,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load("unicode", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let toasts = Arc::new(Mutex::new(Vec::<(Utf16JsonProjection, u64)>::new()));
        let sink = toasts.clone();
        host.dispatch_with_ui_at_session(
            "tool.call",
            test_projection(json!({"tool":"Bash"})),
            &session,
            |_| async { panic!("Mod answers this call") },
            |_, _| async {},
            move |_, text, timeout_ms| {
                let sink = sink.clone();
                async move { sink.lock().await.push((text, timeout_ms)) }
            },
            |_, _| async {},
        )
        .await
        .unwrap();
        let toasts = toasts.lock().await;
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].0.string_units(""), Some(vec![0xd800]));
        assert_eq!(toasts[0].1, 4000);
    }

    #[tokio::test]
    async fn ui_notify_waits_for_host_acceptance_and_preserves_source_text() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("notify.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('ui.notify', ($, e, next) => next({ ...e, text: e.text + '\uD800' }));
            on('tool.call', async ($) => {
              const result = await $.ui.notify('accepted');
              return { result: { resolvedUndefined: result === undefined } };
            });
          }
        "#,
        )
        .unwrap();
        let notices = Arc::new(Mutex::new(Vec::<(String, Utf16JsonProjection)>::new()));
        let session = NotifyTestSessionContext {
            cwd: dir.path().to_path_buf(),
            notices: notices.clone(),
        };
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "notify-mod",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();

        let outcome = host
            .dispatch_with_ui_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                &session,
                |_| async { panic!("Mod answers after the notify operation is accepted") },
                |_, _| async {},
                |_, _, _| async {},
                |_, _| async {},
            )
            .await
            .unwrap();
        assert_eq!(
            mod_test_value(&outcome),
            json!({"result":{"resolvedUndefined":true}})
        );
        let notices = notices.lock().await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "notify-mod");
        assert_eq!(
            notices[0].1.string_units(""),
            Some(vec![
                'a' as u16, 'c' as u16, 'c' as u16, 'e' as u16, 'p' as u16, 't' as u16, 'e' as u16,
                'd' as u16, 0xd800,
            ])
        );
    }

    #[tokio::test]
    async fn ui_status_rewrites_and_clears_without_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("status.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('ui.status', ($, e, next) => next({ text: e.text === 'one' ? 'ONE' : e.text }));
            on('tool.call', ($) => {
              $.ui.status(42);
              $.ui.status('one');
              $.ui.status(undefined);
              return { result: 'done' };
            });
          }
        "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "session".into(),
            turns: 1,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load("status", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let statuses = Arc::new(Mutex::new(
            Vec::<(String, Option<Utf16JsonProjection>)>::new(),
        ));
        let sink = statuses.clone();
        let result = host
            .dispatch_with_ui_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                &session,
                |_| async { panic!("Mod answers this call") },
                |_, _| async {},
                |_, _, _| async {},
                move |plugin, text| {
                    let sink = sink.clone();
                    async move { sink.lock().await.push((plugin, text)) }
                },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":"done"}));
        let statuses = statuses.lock().await;
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].0, "status");
        assert_eq!(
            statuses[0].1.as_ref().and_then(|text| text.string_at("")),
            Some("ONE")
        );
        assert_eq!(statuses[1].0, "status");
        assert!(statuses[1].1.is_none());
    }

    #[tokio::test]
    async fn tool_check_query_runs_mod_chain_without_identity_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("check.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.check', async ($, e, next) => {
              if (e.input.file_path === 'invalid') return { decision: 'allow', reason: 42 };
              if (next.origin.plugin !== 'check' || e.tool_use_id !== undefined) {
                throw new Error('query identity is wrong');
              }
              let pinned = false;
              try { await next({ ...e, tool: 'Bash' }); }
              catch (error) { pinned = String(error).includes('pinned'); }
              const core = await next(e);
              return { decision: pinned && core.decision === 'ask' ? 'allow' : 'deny', rule: 'Read' };
            });
            on('tool.call', async ($, e) => ({ result: await $.tool.check({
              tool: 'Read', input: { file_path: e.file_path ?? 'a.md' }
            }) }));
          }
        "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "session".into(),
            turns: 1,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load("check", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                &session,
                |_| async { panic!("Mod answers the outer call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":{"decision":"allow","rule":"Read"}}));
        let invalid = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash","file_path":"invalid"})),
                &session,
                |_| async { panic!("Mod answers the outer call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let invalid = mod_test_value(&invalid);
        assert_eq!(
            invalid,
            json!({"result":{"decision":"ask","reason":"test policy"}})
        );
    }

    #[tokio::test]
    async fn api_call_skips_only_the_calling_hook_frame() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("self.js");
        std::fs::write(
            &module,
            r#"
          const events = [];
          export function register(on) {
            on('*', ($, e, next) => {
              events.push(next.event);
              if (next.event === 'tool.call' && e.tool === 'Emit') $.ui.log('hello');
              return next(e);
            });
            on('ui.log', ($, e, next) => next({ ...e, text: `${e.text}!` }));
            on('tool.call', { tool: 'Query' }, () => ({ result: events }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("self", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let logs = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
        let sink = logs.clone();
        let answer = host
            .dispatch_with_log(
                "tool.call",
                test_projection(json!({"tool":"Emit"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
                move |plugin, line| {
                    let sink = sink.clone();
                    async move {
                        sink.lock()
                            .await
                            .push((plugin, line.string_at("").unwrap_or_default().to_owned()))
                    }
                },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, json!({"result":"core"}));
        assert_eq!(*logs.lock().await, vec![("self".into(), "hello!".into())]);
        let query = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Query"})),
                |_| async { panic!("the Query hook answers this call") },
            )
            .await
            .unwrap();
        let query = mod_test_value(&query);
        assert_eq!(query["result"], json!(["tool.call", "tool.call"]));
    }

    #[tokio::test]
    async fn clock_now_api_call_is_intercepted_by_mod_event() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("clock.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('clock.now', ($, e, next) => {
              if (next.origin.plugin !== 'clock-test' || next.origin.tier !== 'builtin') {
                throw new Error('wrong API origin');
              }
              return { value: 42 };
            });
            on('tool.call', async ($) => ({ result: await $.clock.now() }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load_with_tier(
            "clock-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
            "builtin",
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { panic!("tool core must be short-circuited") },
            ),
        )
        .await
        .expect("API call must finish")
        .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":42}));
    }

    #[tokio::test]
    async fn clock_sleep_waits_after_mod_rewrites_the_delay() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("clock.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('clock.sleep', ($, e, next) => {
                if (e.ms !== 200 || next.origin.plugin !== 'clock-test') {
                  throw new Error('wrong clock.sleep event');
                }
                return next({ ms: 20 });
              });
              on('tool.call', async ($) => {
                const start = Date.now();
                await $.clock.sleep(200);
                return { result: Date.now() - start };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "clock-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Clock"})),
                |_| async { panic!("tool core must be short-circuited") },
            ),
        )
        .await
        .expect("clock wait finishes")
        .unwrap();
        let result = mod_test_value(&result);
        let elapsed = result["result"].as_u64().unwrap();
        assert!(
            (15..180).contains(&elapsed),
            "rewritten wait was {elapsed} ms"
        );
    }

    #[tokio::test]
    async fn clock_sleep_abort_cancels_one_wait_without_blocking_worker() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("clock.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', async ($, e) => {
                if (e.tool === 'Fast') return { result: 'fast' };
                const cancel = new AbortController();
                const trigger = $.clock.sleep(25).then(() => cancel.abort());
                try {
                  await $.clock.sleep(10000, { signal: cancel.signal });
                  return { result: 'unexpected completion' };
                } catch {
                  await trigger;
                  return { result: 'aborted' };
                }
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "clock-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let interrupted = tokio::time::timeout(
            Duration::from_secs(2),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Slow"})),
                |_| async { panic!("tool core must be short-circuited") },
            ),
        )
        .await
        .expect("abort must end the wait promptly")
        .unwrap();
        let interrupted = mod_test_value(&interrupted);
        assert_eq!(interrupted, json!({"result":"aborted"}));
        let fast = tokio::time::timeout(
            Duration::from_secs(2),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Fast"})),
                |_| async { panic!("tool core must be short-circuited") },
            ),
        )
        .await
        .expect("worker remains usable")
        .unwrap();
        let fast = mod_test_value(&fast);
        assert_eq!(fast, json!({"result":"fast"}));
    }

    #[tokio::test]
    async fn clock_sleep_abort_inside_clock_sleep_interceptor() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("clock.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('clock.sleep', async ($, e, next) => {
                if (e.ms !== 10000) return next(e);
                const cancel = new AbortController();
                const trigger = $.clock.sleep(25).then(() => cancel.abort());
                try {
                  await $.clock.sleep(10000, { signal: cancel.signal });
                  throw new Error('nested wait unexpectedly completed');
                } catch (error) {
                  await trigger;
                  if (error.message === 'nested wait unexpectedly completed') throw error;
                  return next({ ms: 0 });
                }
              });
              on('tool.call', async ($) => {
                await $.clock.sleep(10000);
                return { result: 'nested abort' };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "clock-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Clock"})),
                |_| async { panic!("tool core must be short-circuited") },
            ),
        )
        .await
        .expect("nested abort must end the wait promptly")
        .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":"nested abort"}));
    }

    #[tokio::test]
    async fn clock_timers_survive_hook_settlement_and_unload_cancels_waits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let module = dir.path().join("timers.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              let ticks = 0;
              let interval;
              on('clock.after', ($, e, next) => next({ ms: e.ms === 200 ? 20 : e.ms }));
              on('clock.every', ($, e, next) => next({ ms: 20 }));
              on('tool.call', ($, e) => {
                if (e.tool === 'Ticks') return { result: ticks };
                if (e.tool === 'Schedule') {
                  $.clock.after(200, () => {
                    void $.store.set('after', 'done');
                    $.ui.status('timer fired');
                  });
                  interval = $.clock.every(100, () => {
                    if (++ticks === 3) {
                      interval.cancel();
                      void $.store.set('ticks', ticks);
                    }
                  });
                  return { result: 'scheduled' };
                }
                $.clock.after(500, () => { void $.store.set('late', true); });
                return { result: 'late scheduled' };
              });
            }
        "#,
        )
        .unwrap();
        let session = Arc::new(TimerOutputSession {
            cwd: dir.path().to_path_buf(),
            statuses: std::sync::Mutex::new(Vec::new()),
        });
        let session_trait: Arc<dyn ModSessionContext> = session.clone();
        let host = ModHost::start_with_store_root(None, Some(root))
            .await
            .unwrap();
        host.attach_background_context(Arc::downgrade(&session_trait));
        host.load(
            "timer-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let scheduled = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Schedule"})),
                session.as_ref(),
                |_| async { panic!("Mod handles scheduling") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let scheduled = mod_test_value(&scheduled);
        assert_eq!(scheduled, json!({"result":"scheduled"}));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let after = host
                    .run_store_api_core(
                        "store.get",
                        mod_test_projection(r#"{"key":"after"}"#),
                        "timer-test",
                    )
                    .await
                    .unwrap();
                let ticks = host
                    .run_store_api_core(
                        "store.get",
                        mod_test_projection(r#"{"key":"ticks"}"#),
                        "timer-test",
                    )
                    .await
                    .unwrap();
                if matches!(after, HostApiValue::JsonProjection(value) if value.string_at("") == Some("done"))
                    && matches!(ticks, HostApiValue::JsonProjection(value) if value.number_at("") == Some(3.0))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timer callbacks reached the host after hook settlement");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let ticks = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Ticks"})),
                session.as_ref(),
                |_| async { panic!("Mod handles the query") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let ticks = mod_test_value(&ticks);
        assert_eq!(ticks, json!({"result":3}));
        assert_eq!(
            *session.statuses.lock().unwrap(),
            vec![(
                "timer-test".to_string(),
                Some(test_projection(json!("timer fired")))
            )]
        );
        let late = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Late"})),
                session.as_ref(),
                |_| async { panic!("Mod handles scheduling") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let late = mod_test_value(&late);
        assert_eq!(late, json!({"result":"late scheduled"}));
        host.unload("timer-test").await.unwrap();
        tokio::time::sleep(Duration::from_millis(550)).await;
        let late = host
            .run_store_api_core(
                "store.get",
                mod_test_projection(r#"{"key":"late"}"#),
                "timer-test",
            )
            .await
            .unwrap();
        assert!(matches!(late, HostApiValue::Undefined));
    }

    #[tokio::test]
    async fn clock_after_does_not_fire_after_session_ends() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("timer.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', ($) => {
                $.clock.after(100, () => { void $.store.set('late', true); });
                return { result: 'scheduled' };
              });
            }
        "#,
        )
        .unwrap();
        let session: Arc<dyn ModSessionContext> = Arc::new(TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "ended-session".into(),
            turns: 1,
        });
        let host = ModHost::start_with_store_root(None, Some(dir.path().join("store")))
            .await
            .unwrap();
        host.attach_background_context(Arc::downgrade(&session));
        host.load(
            "timer-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        host.dispatch_with_log_at_session(
            "tool.call",
            test_projection(json!({"tool":"Schedule"})),
            session.as_ref(),
            |_| async { panic!("Mod handles scheduling") },
            |_, _| async {},
        )
        .await
        .unwrap();
        drop(session);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let result = host
            .run_store_api_core(
                "store.get",
                mod_test_projection(r#"{"key":"late"}"#),
                "timer-test",
            )
            .await
            .unwrap();
        assert!(matches!(result, HostApiValue::Undefined));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_run_dispatches_mod_event_and_preserves_host_fields() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("process.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('process.run', ($, e, next) => {
                if (next.origin.plugin !== 'process-test' || e.argv[0] !== '/bin/sh') {
                  throw new Error('wrong process.run event');
                }
                return next({ ...e, init: { ...e.init,
                  env: { ...e.init?.env, MOD_TEST: 'rewritten' } } });
              });
              on('tool.call', async ($, e) => {
                const result = e.tool === 'Failure'
                  ? await $.process.run(['/bin/sh', '-c', 'printf boom >&2; exit 7'],
                      { stdin: 'x'.repeat(65536) })
                  : await $.process.run(['/bin/sh', '-c', 'pwd; printf "%s\\n" "$MOD_TEST"; cat'],
                      { env: { MOD_TEST: 'original' }, stdin: 'body' });
                return { result };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "process-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let output = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Run"})),
                dir.path(),
                |_| async { panic!("Mod handles process.run") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let output = mod_test_value(&output);
        assert_eq!(output["result"]["exitCode"], 0);
        assert_eq!(
            output["result"]["stdout"],
            format!(
                "{}\nrewritten\nbody",
                std::fs::canonicalize(dir.path()).unwrap().display()
            )
        );
        assert_eq!(output["result"]["stderr"], "");
        assert_eq!(output["result"]["isStdoutTruncated"], false);
        let failure = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Failure"})),
                dir.path(),
                |_| async { panic!("Mod handles process.run") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let failure = mod_test_value(&failure);
        assert_eq!(failure["result"]["exitCode"], 7);
        assert_eq!(failure["result"]["stderr"], "boom");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_run_timeout_stops_child_group_and_git_hooks_are_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let environment = HashMap::new();
        let marker = dir.path().join("should-not-exist");
        let command = format!("sleep 1; touch '{}'", marker.display());
        let timeout = run_process_api(
            test_projection(json!({"argv":["/bin/sh","-c",command],"init":{"timeoutMs":50}})),
            dir.path(),
            "process-test",
            &environment,
        )
        .await;
        assert!(
            matches!(timeout, Err(ModError::Hook(message)) if message.contains("still running after 50ms"))
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!marker.exists());
        let git = run_process_api(
            test_projection(json!({"argv":["git","-c","core.hooksPath=/tmp/unsafe","config","--get","core.hooksPath"]})),
            dir.path(),
            "process-test",
            &environment,
        )
        .await
        .unwrap();
        let git = mod_test_value(&git);
        assert_eq!(git["exitCode"], 0);
        assert_eq!(git["stdout"], "/dev/null\n");
        let capped = run_process_api(
            test_projection(json!({"argv":["/bin/sh","-c","yes x | head -c 4194310"],"init":{"timeoutMs":3000}})),
            dir.path(),
            "process-test",
            &environment,
        )
        .await
        .unwrap();
        let capped = mod_test_value(&capped);
        assert_eq!(
            capped["stdout"].as_str().unwrap().len(),
            PROCESS_OUTPUT_LIMIT
        );
        assert_eq!(capped["isStdoutTruncated"], true);
        assert_eq!(decode_process_output(&[b'A', 0xE2, 0x82], true), "A");
        assert_eq!(decode_process_output(&[b'A', 0xE2, 0x82], false), "A�");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_run_stops_when_the_originating_dispatch_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cancelled-child-marker");
        let module = dir.path().join("process.js");
        let marker_json = serde_json::to_string(marker.to_str().unwrap()).unwrap();
        std::fs::write(
            &module,
            format!(
                r#"
                export function register(on) {{
                  on('tool.call', async ($, e) => {{
                    if (e.tool === 'Fast') return {{ result: 'fast' }};
                    await $.process.run(['/bin/sh', '-c', 'sleep 1; touch "$MARKER"'],
                      {{ env: {{ MARKER: {marker_json} }} }});
                    return {{ result: 'unexpected completion' }};
                  }});
                }}
            "#
            ),
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "process-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Slow"})),
                |_| async { panic!("process should be cancelled before the core tool runs") },
            ),
        )
        .await;
        assert!(cancelled.is_err());
        let fast = tokio::time::timeout(
            Duration::from_secs(2),
            host.dispatch(
                "tool.call",
                test_projection(json!({"tool":"Fast"})),
                |_| async { panic!("Mod handles fast call") },
            ),
        )
        .await
        .expect("worker remains responsive")
        .unwrap();
        let fast = mod_test_value(&fast);
        assert_eq!(fast, json!({"result":"fast"}));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(
            !marker.exists(),
            "cancelled process left a descendant running"
        );
    }

    #[tokio::test]
    async fn store_persists_json_and_distinguishes_missing_from_null() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let module = dir.path().join("store.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('store.get', ($, e, next) => e.key === 'intercepted'
                ? { value: 'from-hook' } : next(e));
              on('tool.call', async ($, e) => {
                if (e.tool === 'Write') {
                  await $.store.set('z', { when: new Date('2020-01-02T03:04:05Z') });
                  await $.store.set('2', 2);
                  await $.store.set('1', 1);
                  await $.store.set('nil', null);
                  await $.store.set('canonical', { omitted: undefined, array: [undefined] });
                  let undefinedRootRejected = false;
                  try { await $.store.set('root-undefined', undefined); }
                  catch { undefinedRootRejected = true; }
                  return { result: { written: true, undefinedRootRejected } };
                }
                return { result: {
                  missing: (await $.store.get('absent')) === undefined,
                  nil: await $.store.get('nil'),
                  canonical: await $.store.get('canonical'),
                  intercepted: await $.store.get('intercepted'),
                  value: await $.store.get('z'),
                  keys: await $.store.keys(),
                } };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start_with_store_root(None, Some(root.clone()))
            .await
            .unwrap();
        host.load(
            "store-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let written = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Write"})),
                |_| async { panic!("tool core must be short-circuited") },
            )
            .await
            .unwrap();
        let written = mod_test_value(&written);
        assert_eq!(
            written,
            json!({"result":{"written":true,"undefinedRootRejected":true}})
        );
        let on_disk = std::fs::read_to_string(root.join("store-test.json")).unwrap();
        assert!(on_disk.ends_with("}\n"));
        assert!(on_disk.find("\"1\"").unwrap() < on_disk.find("\"2\"").unwrap());
        drop(host);

        let host = ModHost::start_with_store_root(None, Some(root))
            .await
            .unwrap();
        host.load(
            "store-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let read = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                |_| async { panic!("tool core must be short-circuited") },
            )
            .await
            .unwrap();
        let read = mod_test_value(&read);
        assert_eq!(
            read,
            json!({"result":{
                "missing":true,
                "nil":null,
                "canonical":{"array":[null]},
                "intercepted":"from-hook",
                "value":{"when":"2020-01-02T03:04:05.000Z"},
                "keys":["1","2","z","nil","canonical"]
            }})
        );
    }

    #[tokio::test]
    async fn store_uses_installed_identity_without_collapsing_same_named_mods() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let module_a = dir.path().join("a.js");
        let module_b = dir.path().join("b.js");
        for (module, name) in [(&module_a, "A"), (&module_b, "B")] {
            std::fs::write(
                module,
                format!(
                    r#"
                    export function register(on) {{
                      on('tool.call', async ($, e, next) => {{
                        if (e.tool !== '{name}') return next(e);
                        if (e.action === 'write') await $.store.set('owner', '{name}');
                        return {{ result: {{ name: 'same', owner: await $.store.get('owner') }} }};
                      }});
                    }}
                "#
                ),
            )
            .unwrap();
        }
        let host = ModHost::start_with_store_root(None, Some(root))
            .await
            .unwrap();
        host.load_with_tier_order_storage(
            "same",
            "same@one",
            dir.path(),
            &module_a,
            test_mod_options(json!({})),
            "user",
            None,
        )
        .await
        .unwrap();
        host.load_with_tier_order_storage(
            "same",
            "same@two",
            dir.path(),
            &module_b,
            test_mod_options(json!({})),
            "user",
            None,
        )
        .await
        .unwrap();
        for name in ["A", "B"] {
            let written = host
                .dispatch(
                    "tool.call",
                    test_projection(json!({"tool":name,"action":"write"})),
                    |_| async { panic!("both Mods must stay registered") },
                )
                .await
                .unwrap();
            let written = mod_test_value(&written);
            assert_eq!(written, json!({"result":{"name":"same","owner":name}}));
        }
        host.unload("same@one").await.unwrap();
        let remaining = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"B"})),
                |_| async { panic!("second Mod must survive first unload") },
            )
            .await
            .unwrap();
        let remaining = mod_test_value(&remaining);
        assert_eq!(remaining, json!({"result":{"name":"same","owner":"B"}}));
    }

    #[tokio::test]
    async fn store_serializes_concurrent_writers_and_rejects_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("store");
        let first = ModHost::start_with_store_root(None, Some(root.clone()))
            .await
            .unwrap();
        let second = ModHost::start_with_store_root(None, Some(root))
            .await
            .unwrap();
        let (a, b) = tokio::join!(
            first.run_store_api_core(
                "store.set",
                mod_test_projection(r#"{"key":"a","value":1}"#),
                "shared",
            ),
            second.run_store_api_core(
                "store.set",
                mod_test_projection(r#"{"key":"b","value":2}"#),
                "shared",
            )
        );
        assert!(a.is_ok() && b.is_ok());
        let keys = first
            .run_store_api_core("store.keys", mod_test_projection("{}"), "shared")
            .await
            .unwrap();
        assert!(
            matches!(keys, HostApiValue::JsonProjection(value) if value.array_len("") == Some(2))
        );
        let too_large = "x".repeat(4 * 1024 * 1024);
        let too_large_value = lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
            too_large.clone(),
            too_large.encode_utf16().collect(),
        )
        .unwrap();
        let too_large_input = lingxi_core::types::utf16_json::Utf16JsonProjection::object(vec![
            ("key".into(), mod_test_projection(r#""oversized""#)),
            ("value".into(), too_large_value),
        ])
        .unwrap();
        let error = first
            .run_store_api_core("store.set", too_large_input, "shared")
            .await;
        assert!(matches!(error, Err(ModError::Hook(message)) if message.contains("4 MiB")));
        let missing = second
            .run_store_api_core(
                "store.get",
                mod_test_projection(r#"{"key":"oversized"}"#),
                "shared",
            )
            .await
            .unwrap();
        assert!(matches!(missing, HostApiValue::Undefined));
    }

    #[test]
    fn exact_store_json_stays_in_the_flat_core_carrier() {
        type Ujp = lingxi_core::types::utf16_json::Utf16JsonProjection;
        let exact = Ujp::parse(
            r#"{"loneValue":"\ud800","nested":{"\ud800":"\ud800","\ufffd":"\ufffd","__lingxiModUtf16KeyV1_0__":"ordinary user key"}}"#,
        )
        .unwrap();
        assert_eq!(
            Ujp::parse("1e-6").unwrap().to_json_string().unwrap(),
            "0.000001"
        );
        assert_eq!(
            Ujp::parse("1e-7").unwrap().to_json_string().unwrap(),
            "1e-7"
        );
        assert_eq!(
            Ujp::parse("1e20").unwrap().to_json_string().unwrap(),
            "100000000000000000000"
        );
        assert_eq!(
            Ujp::parse("1e21").unwrap().to_json_string().unwrap(),
            "1e+21"
        );
        assert_eq!(Ujp::parse("-0").unwrap().to_json_string().unwrap(), "0");

        let encoded = exact.to_pretty_json_string().unwrap();
        assert!(encoded.contains(r#""loneValue": "\ud800""#));
        assert!(encoded.contains(r#""\ud800": "\ud800""#));
        assert!(encoded.contains(r#""__lingxiModUtf16KeyV1_0__": "ordinary user key""#));
        let decoded = Ujp::parse(&encoded).unwrap();
        assert!(decoded.semantically_eq(&exact));
        assert_eq!(decoded.string_units("/loneValue").unwrap(), vec![0xd800]);
        let nested = decoded.subprojection("/nested").unwrap();
        let keys = mod_json_object_keys(&nested).unwrap();
        assert!(keys.contains(&vec![0xd800]));
        assert!(keys.contains(&vec![0xfffd]));
        assert!(keys.contains(
            &"__lingxiModUtf16KeyV1_0__"
                .encode_utf16()
                .collect::<Vec<_>>()
        ));

        let absent = Ujp::parse("{}").unwrap();
        let explicit_null = Ujp::parse(r#"{"previous":null}"#).unwrap();
        assert!(mod_optional_exact_json_at_pointer(&absent, "/previous")
            .unwrap()
            .is_none());
        assert!(
            mod_optional_exact_json_at_pointer(&explicit_null, "/previous")
                .unwrap()
                .is_some_and(|value| value.at_pointer("").is_some_and(|root| root.is_null()))
        );
    }

    #[tokio::test]
    async fn store_api_keeps_missing_and_explicit_null_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start_with_store_root(None, Some(dir.path().join("store")))
            .await
            .unwrap();
        let set = host
            .run_store_api_core(
                "store.set",
                mod_test_projection(r#"{"key":"present-null","value":null}"#),
                "null-test",
            )
            .await
            .unwrap();
        assert!(matches!(set, HostApiValue::Undefined));

        let present = host
            .run_store_api_core(
                "store.get",
                mod_test_projection(r#"{"key":"present-null"}"#),
                "null-test",
            )
            .await
            .unwrap();
        assert!(
            matches!(present, HostApiValue::JsonProjection(value) if value.at_pointer("").is_some_and(|root| root.is_null()))
        );

        let missing = host
            .run_store_api_core(
                "store.get",
                mod_test_projection(r#"{"key":"missing"}"#),
                "null-test",
            )
            .await
            .unwrap();
        assert!(matches!(missing, HostApiValue::Undefined));
    }

    #[tokio::test]
    async fn state_api_roundtrips_lone_reference_and_value_units_through_worker() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("state-utf16.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              const lone = String.fromCharCode(0xd800);
              const replacement = '\ufffd';
              const previousExact = [];
              on('state.set', ($, e, next) => {
                if (e.plugin === 'state-utf16-test' && e.key === lone) {
                  previousExact.push(!Object.hasOwn(e, 'previous')
                    || (Object.hasOwn(e.previous, lone) && e.previous[lone] === lone));
                }
                return next(e);
              });
              on('tool.call', async ($) => {
                const loneRef = { plugin: 'state-utf16-test', key: lone };
                const replacementRef = { plugin: 'state-utf16-test', key: replacement };
                await $.state.set(loneRef, { [lone]: lone });
                await $.state.set(loneRef, { [lone]: replacement });
                const nullRef = { plugin: 'state-utf16-test', key: 'explicit-null' };
                const missingRef = { plugin: 'state-utf16-test', key: 'missing' };
                await $.state.set(nullRef, null);
                let undefinedRootRejected = false;
                try { await $.state.set(missingRef, undefined); }
                catch { undefinedRootRejected = true; }
                const explicitNull = await $.state.get(nullRef);
                const missing = await $.state.get(missingRef);
                await $.state.set(replacementRef, { [lone]: 'separate' });
                const loneIdRef = { plugin: 'state-utf16-test', key: 'family', id: lone };
                const replacementIdRef = { plugin: 'state-utf16-test', key: 'family', id: replacement };
                await $.state.set(loneIdRef, 'lone-id');
                await $.state.set(replacementIdRef, 'replacement-id');
                const exact = await $.state.get(loneRef);
                const separate = await $.state.get(replacementRef);
                const loneId = await $.state.get(loneIdRef);
                const replacementId = await $.state.get(replacementIdRef);
                return { result: {
                  loneKeyPresent: Object.hasOwn(exact.value, lone),
                  loneValueStayedDistinct: exact.value[lone] === replacement,
                  stateReferencesDidNotCollide: exact.version === 2 && separate.version === 1,
                  stateFamilyIdsDidNotCollide: loneId.version === 1
                    && replacementId.version === 1
                    && loneId.value === 'lone-id'
                    && replacementId.value === 'replacement-id',
                  previousValuesStayedExact: previousExact.length === 2
                    && previousExact.every(Boolean),
                  explicitNullStaysPresent: Object.hasOwn(explicitNull, 'value')
                    && explicitNull.value === null,
                  missingStateStaysAbsent: !Object.hasOwn(missing, 'value')
                    && missing.version === 0 && undefinedRootRejected,
                } };
              });
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "state-utf16-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"State UTF16"})),
                |_| async { panic!("Mod answers tool.call") },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"],
            json!({
                "loneKeyPresent": true,
                "loneValueStayedDistinct": true,
                "stateReferencesDidNotCollide": true,
                "stateFamilyIdsDidNotCollide": true,
                "previousValuesStayedExact": true,
                "explicitNullStaysPresent": true,
                "missingStateStaysAbsent": true,
            })
        );
    }

    #[tokio::test]
    async fn store_api_roundtrips_lone_keys_values_and_standard_json_disk_shape() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("store-utf16.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              const lone = String.fromCharCode(0xd800);
              const replacement = '\ufffd';
              on('tool.call', async ($) => {
                await $.store.set(lone, { [lone]: lone });
                const first = await $.store.get(lone);
                const firstKeys = await $.store.keys();
                await $.store.set(replacement, { [lone]: replacement });
                const second = await $.store.get(replacement);
                const keys = await $.store.keys();
                await $.store.delete(lone);
                const finalKeys = await $.store.keys();
                return { result: {
                  firstKeyAndValueExact: Object.hasOwn(first, lone) && first[lone] === lone,
                  separateReplacementKey: Object.hasOwn(second, lone)
                    && second[lone] === replacement,
                  firstKeysExact: firstKeys.length === 1 && firstKeys[0] === lone,
                  bothKeysRemainDistinct: keys.length === 2
                    && keys.includes(lone) && keys.includes(replacement),
                  deleteUsedExactKey: finalKeys.length === 1
                    && finalKeys[0] === replacement,
                } };
              });
            }
            "#,
        )
        .unwrap();
        let store_root = dir.path().join("store");
        let host = ModHost::start_with_store_root(None, Some(store_root.clone()))
            .await
            .unwrap();
        host.load(
            "store-utf16-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Store UTF16"})),
                |_| async { panic!("Mod answers tool.call") },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"],
            json!({
                "firstKeyAndValueExact": true,
                "separateReplacementKey": true,
                "firstKeysExact": true,
                "bothKeysRemainDistinct": true,
                "deleteUsedExactKey": true,
            })
        );
        let store_text = std::fs::read_dir(&store_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            })
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .next()
            .expect("store.set persists an ordinary JSON object");
        assert!(store_text.contains("\\ud800"));
        let decoded =
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(&store_text).unwrap();
        assert!(decoded
            .object_entries()
            .unwrap()
            .iter()
            .any(|entry| entry.code_units == [0xfffd]));
    }

    #[test]
    fn store_file_names_keep_safe_names_and_hash_unsafe_names() {
        let root = Path::new("/tmp/store");
        assert_eq!(
            mod_store_file(root, "safe-plugin"),
            root.join("safe-plugin.json")
        );
        assert_eq!(
            mod_store_file(root, "con"),
            root.join("con-1143da2bc54c.json")
        );
        assert!(mod_store_file(root, "a/b")
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("a_b-"));
    }

    #[tokio::test]
    async fn session_cwd_api_uses_live_cwd_and_dispatches_empty_input() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("cwd.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('session.cwd', async ($, e, next) => {
              if (Object.keys(e).length !== 0 || next.origin.plugin !== 'cwd-test') {
                throw new Error('wrong session.cwd event');
              }
              const core = await next(e);
              return { value: core.value + '/intercepted' };
            });
            on('tool.call', async ($) => ({ result: await $.session.cwd() }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("cwd-test", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                dir.path(),
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result,
            json!({"result":format!("{}/intercepted", dir.path().display())})
        );
    }

    #[tokio::test]
    async fn session_repo_api_reads_live_git_remote_and_main_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&[
            "config",
            "remote.origin.url",
            "https://fetch.example/team/repo.git",
        ]);
        git(&[
            "config",
            "remote.origin.pushurl",
            "https://user:secret@push.example/team/repo.git",
        ]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-qm",
            "init",
        ]);
        let linked = dir.path().join("linked");
        git(&["worktree", "add", "-qb", "linked", linked.to_str().unwrap()]);

        let module = dir.path().join("repo.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('session.repo', ($, e, next) => {
                if (Object.keys(e).length !== 0 || next.origin.plugin !== 'repo-test') {
                  throw new Error('wrong session.repo event');
                }
                return next(e);
              });
              on('tool.call', async ($) => ({ result: await $.session.repo() }));
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "repo-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Query"})),
                &linked,
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result,
            json!({"result": {
                "root": std::fs::canonicalize(&repo).unwrap().to_string_lossy(),
                "remote": "https://push.example/team/repo.git",
                "internal": false,
                "name": null,
            }})
        );
        assert_eq!(session_repo(dir.path()).await, Value::Null);
    }

    #[tokio::test]
    async fn prompt_section_rewrites_text_and_keeps_its_name_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("section.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('prompt.section', { name: 'memory' }, ($, e, next) =>
                next({ ...e, text: e.text + ' edited' }));
              on('prompt.section', { name: 'pronouns' }, () => ({ text: null }));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "section-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let changed = host
            .dispatch(
                "prompt.section",
                test_projection(json!({"name":"memory","text":"base"})),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    Ok(Some(test_projection(json!({"text":event["text"]}))))
                },
            )
            .await
            .unwrap();
        let changed = mod_test_value(&changed);
        assert_eq!(changed, json!({"text":"base edited"}));
        let removed = host
            .dispatch(
                "prompt.section",
                test_projection(json!({"name":"pronouns","text":"base"})),
                |_| async { panic!("direct section answer skips core") },
            )
            .await
            .unwrap();
        let removed = mod_test_value(&removed);
        assert_eq!(removed, json!({"text":null}));
    }

    #[tokio::test]
    async fn tool_describe_preserves_defer_override_and_pins_tool_provider() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("describe.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('tool.describe', { tool: 'Read' }, async ($, e, next) => {
                const below = await next({ ...e, description: e.description + ' below', isDeferred: true });
                return { description: below.description + ' outer' };
              });
              on('tool.describe', { tool: 'Write' }, ($, e, next) =>
                next({ ...e, tool: 'Read', provider: { plugin: 'evil', tier: 'user' } }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("describe", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let provider = json!({"plugin":"engine","tier":"core"});
        let read = host
            .dispatch(
                "tool.describe",test_projection(json!({"tool":"Read","description":"base","provider":provider})),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    Ok(Some(test_projection(json!({"description":event["description"],"isDeferred":event["isDeferred"]}))))
                },
            )
            .await
            .unwrap();
        let read = mod_test_value(&read);
        assert_eq!(
            read,
            json!({"description":"base below outer","isDeferred":true})
        );
        let write = host
            .dispatch(
                "tool.describe",
                test_projection(
                    json!({"tool":"Write","description":"original","provider":provider}),
                ),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    Ok(Some(test_projection(
                        json!({"description":event["description"]}),
                    )))
                },
            )
            .await
            .unwrap();
        let write = mod_test_value(&write);
        assert_eq!(write, json!({"description":"original"}));
    }

    #[tokio::test]
    async fn sec_default_tool_describe_bypasses_user_for_org_mcp_and_keeps_normal_routes() {
        let dir = tempfile::tempdir().unwrap();
        let user_module = dir.path().join("user-describe.js");
        std::fs::write(
            &user_module,
            r#"export function register(on) {
              on('tool.describe', { tool: 'mcp__org__authenticate' }, async ($, e, next) => {
                const result = await next({ ...e, description: e.description + ':user' });
                return { description: result.description };
              });
            }"#,
        )
        .unwrap();
        let append_module = dir.path().join("append-describe.js");
        std::fs::write(
            &append_module,
            r#"export function register(on) {
              on('tool.describe', { tool: 'mcp__org__authenticate' }, async ($, e, next) => {
                const result = await next(e);
                return { description: result.description + ':append' };
              });
            }"#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        host.load(
            "user-describe",
            dir.path(),
            &user_module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        host.load_with_tier_order(
            "append-describe",
            dir.path(),
            &append_module,
            test_mod_options(json!({})),
            "append",
            Some(0),
        )
        .await
        .unwrap();
        host.set_sec_default_order(Some(-1));

        // Both Native organization scopes reach this handler as prepend-tier
        // providers; the worker must not let the user tier rewrite them.
        for scope in ["managed", "enterprise"] {
            let answer = host
                .dispatch(
                    "tool.describe",
                    test_projection(json!({
                        "tool":"mcp__org__authenticate",
                        "description":"base",
                        "provider":{"plugin":"mcp:org","tier":"prepend"}
                    })),
                    |event| async move {
                        let event = mod_test_event(event.clone());
                        Ok(Some(test_projection(
                            json!({"description":event["description"]}),
                        )))
                    },
                )
                .await
                .unwrap();
            let answer = mod_test_value(&answer);
            assert_eq!(
                answer,
                json!({"description":"base:append"}),
                "scope={scope}"
            );
        }

        for (plugin, tier) in [
            ("mcp:personal", "user"),
            ("native-tool", "builtin"),
            ("engine", "core"),
        ] {
            let answer = host
                .dispatch(
                    "tool.describe",
                    test_projection(json!({
                        "tool":"mcp__org__authenticate",
                        "description":"base",
                        "provider":{"plugin":plugin,"tier":tier}
                    })),
                    |event| async move {
                        let event = mod_test_event(event.clone());
                        Ok(Some(test_projection(
                            json!({"description":event["description"]}),
                        )))
                    },
                )
                .await
                .unwrap();
            let answer = mod_test_value(&answer);
            assert_eq!(
                answer,
                json!({"description":"base:user:append"}),
                "tier={tier}"
            );
        }
    }

    #[tokio::test]
    async fn command_describe_rewrites_menu_fields_and_pins_identity() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("command-describe.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('command.describe', { command: 'hello' }, async ($, e, next) => {
                const below = await next({ ...e, description: 'below', isHidden: true, provider: undefined });
                return { description: below.description + ' outer', isHidden: below.isHidden };
              });
              on('command.describe', { command: 'bad' }, ($, e, next) =>
                next({ ...e, immediate: true, provider: { plugin: 'evil', tier: 'user' } }));
              on('command.describe', { command: 'long' }, () =>
                ({ description: 'x'.repeat(4097), isHidden: false }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "command-describe",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let provider = json!({"plugin":"engine","tier":"core"});
        let hello = host
            .dispatch(
                "command.describe",test_projection(json!({"command":"hello","description":"base","argumentHint":"[name]","isHidden":false,"immediate":false,"provider":provider})),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    assert_eq!(event["provider"], json!({"plugin":"engine","tier":"core"}));
                    assert_eq!(event["command"], "hello");
                    Ok(Some(test_projection(json!({"description":event["description"],"argumentHint":event["argumentHint"],"isHidden":event["isHidden"]}))))
                },
            )
            .await
            .unwrap();
        let hello = mod_test_value(&hello);
        assert_eq!(hello, json!({"description":"below outer","isHidden":true}));
        for name in ["bad", "long"] {
            let answer = host
                .dispatch(
                    "command.describe",test_projection(json!({"command":name,"description":"base","isHidden":false,"immediate":false,"provider":provider})),
                    |event| async move {
                    let event = mod_test_event(event.clone());
                        Ok(Some(test_projection(json!({"description":event["description"],"isHidden":event["isHidden"]}))))
                    },
                )
                .await
                .unwrap();
            let answer = mod_test_value(&answer);
            assert_eq!(answer, json!({"description":"base","isHidden":false}));
        }
    }

    #[tokio::test]
    async fn prompt_attachment_rewrites_body_or_omits_without_changing_origin() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("attachment.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('prompt.attachment', { type: 'todo_reminder' }, ($, e, next) =>
                next({ ...e, text: e.text + ' edited', origin: undefined }));
              on('prompt.attachment', { type: 'skill_listing' }, () => ({ text: null }));
              on('prompt.attachment', { type: 'plan_mode' }, ($, e, next) =>
                next({ ...e, type: 'todo_reminder', origin: { kind: 'plugin', event: 'tool.call' } }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "attachment",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let changed = host
            .dispatch(
                "prompt.attachment",
                test_projection(
                    json!({"type":"todo_reminder","text":"body","origin":{"kind":"engine"}}),
                ),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    assert_eq!(event["origin"], json!({"kind":"engine"}));
                    Ok(Some(test_projection(json!({"text":event["text"]}))))
                },
            )
            .await
            .unwrap();
        let changed = mod_test_value(&changed);
        assert_eq!(changed, json!({"text":"body edited"}));
        let omitted = host
            .dispatch(
                "prompt.attachment",
                test_projection(
                    json!({"type":"skill_listing","text":"skills","origin":{"kind":"engine"}}),
                ),
                |_| async { panic!("omitted attachment does not reach core") },
            )
            .await
            .unwrap();
        let omitted = mod_test_value(&omitted);
        assert_eq!(omitted, json!({"text":null}));
        let pinned = host
            .dispatch(
                "prompt.attachment",
                test_projection(
                    json!({"type":"plan_mode","text":"plan","origin":{"kind":"engine"}}),
                ),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    Ok(Some(test_projection(json!({"text":event["text"]}))))
                },
            )
            .await
            .unwrap();
        let pinned = mod_test_value(&pinned);
        assert_eq!(pinned, json!({"text":"plan"}));
    }

    #[tokio::test]
    async fn prompt_context_rerenders_instruction_files_for_downstream_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("context.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('prompt.context', ($, e, next) => next({
                ...e,
                instructionFiles: [{ path: '/project/AGENTS.md', kind: 'project', content: 'new rules' }],
              }));
              on('prompt.context', { instructionFiles: { kind: 'project' } }, ($, e, next) => {
                if (!e.blocks.find(block => block.name === 'claudeMd')?.text.includes('new rules')) {
                  throw new Error('instruction files were not rendered before this hook');
                }
                return next(e);
              });
            }
            "#,
        ).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "context-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "prompt.context",
                test_projection(
                    json!({"blocks":[{"name":"currentDate","text":"Today"}],"instructionFiles":[]}),
                ),
                |event| async move { Ok(event) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["instructionFiles"][0]["path"], "/project/AGENTS.md");
        assert_eq!(result["blocks"][0]["name"], "claudeMd");
        assert_eq!(
            result["blocks"][0]["text"],
            format!(
                "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.\n\nContents of /project/AGENTS.md (project instructions, checked into the codebase):\n\nnew rules"
            )
        );
    }

    #[tokio::test]
    async fn env_get_requires_a_scanned_literal_and_dispatches_interceptors() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("env.js");
        let missing = format!(
            "{ENV_PREFIX}MOD_UNSET_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let source = r#"
            export function register(on) {
              on('env.get', async ($, e, next) => {
                const core = await next(e);
                if (e.name === 'HOME') return { value: typeof core.value === 'string' ? core.value + '!' : 'absent' };
                return core;
              });
              on('tool.call', async ($) => {
                const home = await $.env.get('HOME');
                let denied = false;
                try { await $.env.get(['PA', 'TH'].join('')); }
                catch { denied = true; }
                const missing = await $.env.get('__UNSET__');
                return { result: { home, denied, missingWasUndefined: missing === undefined } };
              });
            }
            "#
        .replace("__UNSET__", &missing);
        std::fs::write(&module, source).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("env-test", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                dir.path(),
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        let expected_home = std::env::var_os("HOME")
            .map(|value| format!("{}!", value.to_string_lossy()))
            .unwrap_or_else(|| "absent".into());
        assert_eq!(result["result"]["home"], expected_home);
        assert_eq!(result["result"]["denied"], true);
        assert_eq!(result["result"]["missingWasUndefined"], true);
    }

    #[tokio::test]
    async fn env_set_uses_scanned_write_grants_and_updates_session_processes() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("env_set.js");
        let name = format!(
            "{ENV_PREFIX}MOD_ENV_SET_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let source = r#"
            export function register(on) {
              on('env.set', { name: '__NAME__' }, async ($, e, next) => {
                if (e.value === undefined) return next(e);
                let pinned = false;
                try { await next({ ...e, name: e.name + '_OTHER' }); }
                catch (error) { pinned = String(error.message).includes('name is pinned'); }
                if (!pinned) throw new Error('env.set changed the variable name');
                return next({ ...e, value: e.value + '!' });
              });
              on('tool.call', async ($) => {
                await $.env.set('__NAME__', 'value');
                const got = await $.env.get('__NAME__');
                const child = await $.process.run(['/bin/sh', '-c', 'printf %s "$__NAME__"']);
                const explicit = await $.process.run(['/bin/sh', '-c', 'printf %s "$__NAME__"'],
                  { env: { '__NAME__': 'explicit' } });
                let nulDenied = false;
                try { await $.env.set('__NAME__', 'bad\0value'); }
                catch (error) { nulDenied = String(error.message).includes('without NUL'); }
                let denied = false;
                try { await $.env.set(['__PREFIX__', '__SUFFIX__'].join('') + '_OTHER', 'bad'); }
                catch (error) { denied = String(error.message).includes('env.set refused'); }
                await $.env.set('__NAME__');
                const missing = await $.env.get('__NAME__');
                const unsetChild = await $.process.run(['/bin/sh', '-c', 'printf %s "${__NAME__-unset}"']);
                return { result: { got, child: child.stdout, explicit: explicit.stdout,
                  denied, nulDenied,
                  missingWasUndefined: missing === undefined, unsetChild: unsetChild.stdout } };
              });
            }
        "#
        .replace("__PREFIX__", &name[..name.len() / 2])
        .replace("__SUFFIX__", &name[name.len() / 2..])
        .replace("__NAME__", &name);
        std::fs::write(&module, source).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "env-set-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                dir.path(),
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"]["got"], "value!");
        assert_eq!(result["result"]["child"], "value!");
        assert_eq!(result["result"]["explicit"], "explicit");
        assert_eq!(result["result"]["denied"], true);
        assert_eq!(result["result"]["nulDenied"], true);
        assert_eq!(result["result"]["missingWasUndefined"], true);
        assert_eq!(result["result"]["unsetChild"], "unset");
        assert!(std::env::var_os(name).is_none());
    }

    #[tokio::test]
    async fn env_literal_grants_are_scoped_to_the_loading_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.js");
        let second = dir.path().join("second.js");
        std::fs::write(
            &first,
            r#"export function register(on) {
              on('tool.call', { tool: 'First' }, async ($) => ({ result: await $.env.get('HOME') }));
            }"#,
        ).unwrap();
        std::fs::write(
            &second,
            r#"export function register(on) {
              on('tool.call', { tool: 'Second' }, async ($) => {
                const name = ['HO', 'ME'].join('');
                try { await $.env.get(name); return { result: 'leaked' }; }
                catch { return { result: 'refused' }; }
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("first", dir.path(), &first, test_mod_options(json!({})))
            .await
            .unwrap();
        host.load("second", dir.path(), &second, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Second"})),
                dir.path(),
                |_| async { panic!("Second Mod answers") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"], "refused");
    }

    #[tokio::test]
    async fn reloading_a_mod_drops_its_previous_env_grants() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("reload.js");
        std::fs::write(
            &module,
            "export function register(on) { on('tool.call', async ($) => ({ result: await $.env.get('HOME') })); }",
        ).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("reload", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        std::fs::write(
            &module,
            "export function register(on) { on('tool.call', async ($) => { const name = ['HO', 'ME'].join(''); try { await $.env.get(name); return { result: 'leaked' }; } catch { return { result: 'refused' }; } }); }",
        ).unwrap();
        host.load("reload", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                dir.path(),
                |_| async { panic!("reloaded Mod answers") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"], "refused");
    }

    #[tokio::test]
    async fn typescript_type_only_claude_code_import_loads() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("typed.ts");
        std::fs::write(
            &module,
            r#"
            import type { On } from 'claude-code';
            export function register(on: On) {
              on('prompt.context', ($, e, next) => next(e));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("typed", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let result = host
            .dispatch(
                "prompt.context",
                test_projection(json!({"blocks":[],"instructionFiles":[]})),
                |event| async move { Ok(event) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"blocks":[],"instructionFiles":[]}));
    }

    #[tokio::test]
    async fn prompt_context_text_rewrite_makes_instruction_files_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("context.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('prompt.context', ($, e, next) => next({
                ...e,
                blocks: [{ name: 'claudeMd', text: 'custom instructions' }],
              }));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "context-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "prompt.context",
                test_projection(
                    json!({"blocks":[{"name":"claudeMd","text":"original"}],"instructionFiles":[]}),
                ),
                |event| async move { Ok(event) },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["blocks"][0]["text"], "custom instructions");
        assert!(result.get("instructionFiles").is_none());
    }

    #[tokio::test]
    async fn session_messages_dispatches_operation_and_uses_session() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("messages.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('session.messages', async ($, e, next) => {
                if (Object.keys(e).length !== 0 || next.origin.plugin !== 'messages-test') {
                  throw new Error('wrong session.messages input');
                }
                const result = await next(e);
                return { value: result.value.map(row => ({ ...row, text: row.text + '!' })) };
              });
              on('tool.call', async ($) => ({ result: await $.session.messages() }));
            }
            "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "messages-session".into(),
            turns: 0,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "messages-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"],
            json!([{"role":"user","text":"hello!","toolUses":[]}])
        );
    }

    #[tokio::test]
    async fn session_messages_api_option_and_argument_errors_match_current_surface() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("messages_api.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('session.messages', { as: 'api' }, async ($, e, next) => {
                const result = await next(e);
                return { value: result.value.map(row => ({ ...row, seen: true })) };
              });
              on('tool.call', async ($) => {
                const api = await $.session.messages({ as: 'api' });
                const errors = [];
                for (const args of [null, { as: 'raw' }, { agentId: '' }, { extra: true }]) {
                  try { await $.session.messages(args); errors.push('accepted'); }
                  catch (error) { errors.push(String(error.message)); }
                }
                return { result: { api, errors } };
              });
            }
        "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "messages-api-session".into(),
            turns: 0,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "messages-api-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"]["api"],
            json!([{"role":"user","content":[
            {"type":"text","text":"hello"}],"seen":true}])
        );
        let errors = result["result"]["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 4);
        assert!(errors.iter().all(|error| {
            error
                .as_str()
                .is_some_and(|text| text.contains("$.session.messages takes"))
        }));
    }

    #[tokio::test]
    async fn settings_read_dispatches_interceptable_source_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("settings.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('settings.read', async ($, e, next) => {
                if (e.source !== 'policy' || next.origin.plugin !== 'settings-test') {
                  throw new Error('wrong settings.read input');
                }
                const result = await next(e);
                return { value: { ...result.value, observed: true } };
              });
              on('tool.call', async ($) => {
                try {
                  await $.settings.read(null);
                  throw new Error('settings.read(null) was accepted');
                } catch (error) {
                  if (!String(error.message).includes('takes { source } or nothing')) throw error;
                }
                return { result: await $.settings.read({ source: 'policy', ignored: true }) };
              });
            }
            "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "settings-session".into(),
            turns: 0,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "settings-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"], json!({"source":"policy","observed":true}));
    }

    #[tokio::test]
    async fn sec_default_settings_read_skips_user_interceptor() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        for (name, tier, source) in [
            (
                "caller",
                "user",
                r#"
                export function register(on) {
                  on('tool.call', async ($) => ({ result: await $.settings.read({ source: 'policy' }) }));
                }
            "#,
            ),
            (
                "rewrite",
                "user",
                r#"
                export function register(on) {
                  on('settings.read', () => ({ value: { tampered: true } }));
                }
            "#,
            ),
            (
                "audit",
                "append",
                r#"
                export function register(on) {
                  on('settings.read', async ($, e, next) => {
                    const result = await next(e);
                    return { value: { ...result.value, audited: true } };
                  });
                }
            "#,
            ),
        ] {
            let module = dir.path().join(format!("{name}.js"));
            std::fs::write(&module, source).unwrap();
            host.load_with_tier(name, dir.path(), &module, test_mod_options(json!({})), tier)
                .await
                .unwrap();
        }
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "settings-session".into(),
            turns: 0,
        };
        host.set_sec_default_order(Some(-1));
        let guarded = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let guarded = mod_test_value(&guarded);
        assert_eq!(guarded["result"], json!({"source":"policy","audited":true}));
        host.set_sec_default_order(None);
        let unguarded = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let unguarded = mod_test_value(&unguarded);
        assert_eq!(unguarded["result"], json!({"tampered":true}));
    }

    #[tokio::test]
    async fn sec_default_tool_check_keeps_managed_rule_deny_unless_policy_allows_override() {
        for (session_id, expected, core_calls, notice_count, core_answer) in [
            (
                "managed-deny",
                "deny",
                1,
                1,
                json!({"decision":"deny","rule":"Read(secret)"}),
            ),
            (
                "allow-override",
                "allow",
                0,
                0,
                json!({"decision":"deny","rule":"Read(secret)"}),
            ),
            ("ordinary-ask", "allow", 1, 0, json!({"decision":"ask"})),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let module = dir.path().join("lift.js");
            std::fs::write(
                &module,
                r#"
                export function register(on) {
                  on('tool.check', () => ({ decision: 'allow' }));
                }
                "#,
            )
            .unwrap();
            let host = ModHost::start(None).await.unwrap();
            host.load("lift", dir.path(), &module, test_mod_options(json!({})))
                .await
                .unwrap();
            host.set_sec_default_order(Some(-1));
            let session = TestSessionContext {
                cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
                root: dir.path().to_path_buf(),
                model: std::sync::Mutex::new("test".into()),
                id: session_id.into(),
                turns: 0,
            };
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let logs = Arc::new(StdMutex::new(Vec::new()));
            let calls_for_core = calls.clone();
            let logs_for_host = logs.clone();
            let result = host
                .dispatch_with_log_at_session(
                    "tool.check",
                    test_projection(
                        json!({"tool":"Read","input":{"file_path":"secret"},"tool_use_id":"u1"}),
                    ),
                    &session,
                    move |_| {
                        calls_for_core.fetch_add(1, Ordering::Relaxed);
                        let answer = core_answer.clone();
                        async move { Ok(Some(test_projection(answer))) }
                    },
                    move |plugin, text| {
                        let logs = logs_for_host.clone();
                        async move {
                            logs.lock()
                                .unwrap()
                                .push((plugin, text.string_at("").unwrap_or_default().to_owned()))
                        }
                    },
                )
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result["decision"], expected);
            assert_eq!(calls.load(Ordering::Relaxed), core_calls);
            let captured = logs.lock().unwrap();
            assert_eq!(captured.len(), notice_count);
            if notice_count > 0 {
                assert_eq!(captured[0].0, "cc-plugin-sec-default");
                assert_eq!(
                    captured[0].1,
                    "lift tried to lift a deny rule in your settings from a Read call (Read(secret)); the deny rule holds over the plugins you install (allowModsToOverrideDenyRules)"
                );
            }
            drop(captured);
            if session_id == "managed-deny" {
                let logged = logs.clone();
                let again = host
                    .dispatch_with_log_at_session(
                        "tool.check",test_projection(json!({"tool":"Read","input":{"file_path":"secret"},"tool_use_id":"u2"})),
                        &session,
                        |_| async { Ok(Some(test_projection(json!({"decision":"deny","rule":"Read(secret)"})))) },
                        move |plugin, text| {
                            let logged = logged.clone();
                            async move {
                                logged.lock().unwrap().push((
                                    plugin,
                                    text.string_at("").unwrap_or_default().to_owned(),
                                ))
                            }
                        },
                    )
                    .await
                    .unwrap();
                let again = mod_test_value(&again);
                assert_eq!(again["decision"], "deny");
                assert_eq!(logs.lock().unwrap().len(), 1, "notice is once per plugin");
            }
        }
    }

    #[tokio::test]
    async fn sec_default_holds_synthetic_nested_bash_deny_projection_over_user_allow() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("lift.js");
        std::fs::write(
            &module,
            "export function register(on) { on('tool.check', () => ({ decision: 'allow' })); }",
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("lift", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        host.set_sec_default_order(Some(-1));
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "nested-managed-deny".into(),
            turns: 0,
        };
        let logs = Arc::new(StdMutex::new(Vec::new()));
        let logs_for_host = logs.clone();

        // Synthetic projection seam: the permission unit test proves that a
        // nested same-behavior Bash rule yields this `rule` string. This test
        // exercises the real ModHost/sec-default/user-plugin projection gate;
        // it does not claim the current production permission policy constructs
        // nested SubcommandResults.
        let result = host
            .dispatch_with_log_at_session(
                "tool.check",
                test_projection(json!({
                    "tool":"Bash",
                    "input":{"command":"echo ok && rm -rf target"},
                    "tool_use_id":"nested-u1"
                })),
                &session,
                |_| async {
                    Ok(Some(test_projection(
                        json!({"decision":"deny","rule":"Bash(rm:*)"}),
                    )))
                },
                move |plugin, text| {
                    let logs = logs_for_host.clone();
                    async move {
                        logs.lock()
                            .unwrap()
                            .push((plugin, text.string_at("").unwrap_or_default().to_owned()))
                    }
                },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);

        assert_eq!(result["decision"], "deny");
        assert_eq!(result["rule"], "Bash(rm:*)");
        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, "cc-plugin-sec-default");
        assert!(logs[0].1.contains("Bash(rm:*)"));
    }

    #[tokio::test]
    async fn sec_default_tool_check_refuses_when_managed_rule_recheck_fails() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("allow.js");
        std::fs::write(
            &module,
            "export function register(on) { on('tool.check', () => ({ decision: 'allow' })); }",
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("allow", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        host.set_sec_default_order(Some(-1));
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "managed-deny".into(),
            turns: 0,
        };
        let result = host
            .dispatch_with_log_at_session(
                "tool.check",
                test_projection(
                    json!({"tool":"Read","input":{"file_path":"secret"},"tool_use_id":"u2"}),
                ),
                &session,
                |_| async { Err(ModError::Hook("core policy unavailable".into())) },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result,
            json!({"decision":"deny","reason":"the deny rules in your settings could not be checked for this call, so it is refused"})
        );
    }

    #[tokio::test]
    async fn sec_default_tool_register_obeys_managed_server_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', async ($) => {
                try {
                  return { result: await $.tool.register({ name: 'echo', description: 'Echo' }) };
                } catch (error) {
                  return { result: String(error.message) };
                }
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("user-mod", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        host.set_sec_default_order(Some(-1));
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "org-tools".into(),
            turns: 0,
        };
        let answer = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert!(answer["result"]
            .as_str()
            .unwrap()
            .contains("allowedMcpServers (managed)"));
    }

    #[tokio::test]
    async fn sec_default_tool_register_refuses_when_policy_read_fails() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('tool.call', async ($) => {
                try { return { result: await $.tool.register({ name: 'echo', description: 'Echo' }) }; }
                catch (error) { return { result: String(error.message) }; }
              });
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("user-mod", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        host.set_sec_default_order(Some(-1));
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "policy-read-error".into(),
            turns: 0,
        };
        let answer = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert!(answer["result"]
            .as_str()
            .unwrap()
            .contains("allowedMcpServers (managed)"));
    }

    #[tokio::test]
    async fn tool_register_preserves_optional_is_deferred_through_guest_api() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', async ($) => {
                const omitted = await $.tool.register({ name: 'omitted', description: 'Omitted' });
                const deferred = await $.tool.register({ name: 'deferred', description: 'Deferred', isDeferred: true });
                const immediate = await $.tool.register({ name: 'immediate', description: 'Immediate', isDeferred: false });
                let invalid;
                try {
                  await $.tool.register({ name: 'invalid', description: 'Invalid', isDeferred: 'false' });
                } catch (error) {
                  invalid = String(error.message);
                }
                return { result: { omitted, deferred, immediate, invalid } };
              });
            }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load_with_tier(
            "register-is-deferred",
            dir.path(),
            &module,
            test_mod_options(json!({})),
            "builtin",
        )
        .await
        .unwrap();
        host.set_sec_default_order(Some(-1));
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "tool-register-is-deferred".into(),
            turns: 0,
        };

        let answer = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(
            answer["result"]["omitted"],
            json!({
                "tool":"mcp__register-is-deferred__omitted",
                "inputSchema":{"type":"object"}
            })
        );
        assert_eq!(answer["result"]["deferred"]["isDeferred"], json!(true));
        assert_eq!(answer["result"]["immediate"]["isDeferred"], json!(false));
        assert!(answer["result"]["invalid"]
            .as_str()
            .unwrap()
            .contains("isDeferred must be true or false"));
    }

    #[tokio::test]
    async fn sec_default_plugin_register_refuses_user_and_keeps_managed_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.set_sec_default_order(Some(-1));
        let session: Arc<dyn ModSessionContext> = Arc::new(TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "managed-mods-only".into(),
            turns: 0,
        });
        host.attach_background_context(Arc::downgrade(&session));
        let candidate = |tier: &str| {
            json!({
                "name":"candidate","tier":tier,"root":dir.path(),"version":"1.0.0",
                "provenance":"candidate@inline","uses":{"events":[],"calls":[]}
            })
        };
        let denied = host
            .dispatch_plugin_register(test_projection(candidate("user")))
            .await
            .unwrap();
        assert!(mod_test_value(&denied)["refuse"]
            .as_str()
            .unwrap()
            .contains("allowManagedModsOnly"));
        let allowed = host
            .dispatch_plugin_register(test_projection(candidate("prepend")))
            .await
            .unwrap();
        assert_eq!(mod_test_value(&allowed), json!({"allow":true}));

        let unavailable = ModHost::start(None).await.unwrap();
        unavailable.set_sec_default_order(Some(-1));
        let failing_session: Arc<dyn ModSessionContext> = Arc::new(TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "policy-read-error".into(),
            turns: 0,
        });
        unavailable.attach_background_context(Arc::downgrade(&failing_session));
        let denied = unavailable
            .dispatch_plugin_register(test_projection(candidate("user")))
            .await
            .unwrap();
        assert!(mod_test_value(&denied)["refuse"]
            .as_str()
            .unwrap()
            .contains("allowManagedModsOnly"));
    }

    #[tokio::test]
    async fn prompt_submit_routes_text_and_as_user_to_session_queue() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("submit.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('session.start', async ($, e, next) => {
                const blank = await $.prompt.submit({ text: '  ' }).catch(error => error.message);
                const slash = await $.prompt.submit({ text: '  /help' }).catch(error => error.message);
                const attached = await $.prompt.submit({ text: 'hello', attachments: [{}] }).catch(error => error.message);
                const invalidAsUser = await $.prompt.submit({ text: 'hello', asUser: 'yes' }).catch(error => error.message);
                const queued = await $.prompt.submit({ text: 'follow up', asUser: true });
                return { ...await next(e), blank, slash, attached, invalidAsUser, queued };
              });
              on('tool.call', async ($) => ({
                result: await $.prompt.submit({ text: 'blocked' }).catch(error => error.message)
              }));
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("submit", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "submit-test".into(),
            turns: 0,
        };
        let input = json!({"cwd":dir.path().to_string_lossy()});
        let answer = host
            .dispatch_with_log_at_session(
                "session.start",
                test_projection(input),
                &session,
                |event| async { Ok(event) },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert!(answer["blank"]
            .as_str()
            .unwrap()
            .contains("non-empty prompt"));
        assert!(answer["slash"].as_str().unwrap().contains("$.command.run"));
        assert!(answer["attached"]
            .as_str()
            .unwrap()
            .contains("attachments cannot be submitted"));
        assert!(answer["invalidAsUser"].as_str().unwrap().contains("asUser"));
        assert_eq!(
            answer["queued"],
            json!({"text":"follow up","plugin":"submit","asUser":true})
        );
        let blocked = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let blocked = mod_test_value(&blocked);
        assert!(blocked["result"]
            .as_str()
            .unwrap()
            .contains("turn this hook is holding"));
    }

    #[tokio::test]
    async fn session_receive_matches_origin_rewrites_and_can_consume() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("receive.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('session.receive', { origin: 'peer' }, ($, e, next) =>
                next({ ...e, text: e.text + ' rewritten' }));
              on('session.receive', { origin: 'task-notification' }, () =>
                ({ consumed: 'muted' }));
              on('session.receive', { origin: 'bridge' }, async ($, e, next) =>
                ({ consumed: await next({ ...e, origin: { kind: 'peer' } })
                  .then(() => 'forgery reached core', error => error.message) }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("receive", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let queued = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = queued.clone();
        let rewritten = host
            .dispatch(
                "session.receive",
                test_projection(json!({"origin":{"kind":"peer"},"text":"peer"})),
                move |event| {
                    let event = mod_test_event(event.clone());
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(event["text"].clone());
                        Ok(Some(test_projection(json!({"text":event["text"]}))))
                    }
                },
            )
            .await
            .unwrap();
        let rewritten = mod_test_value(&rewritten);
        assert_eq!(rewritten, json!({"text":"peer rewritten"}));
        assert_eq!(*queued.lock().unwrap(), vec![json!("peer rewritten")]);
        let consumed = host
            .dispatch(
                "session.receive",
                test_projection(json!({"origin":{"kind":"task-notification"},"text":"notice"})),
                |_| async { panic!("consumed delivery must not reach core") },
            )
            .await
            .unwrap();
        let consumed = mod_test_value(&consumed);
        assert_eq!(consumed, json!({"consumed":"muted"}));
        let forged = host
            .dispatch(
                "session.receive",
                test_projection(json!({"origin":{"kind":"bridge"},"text":"bridge"})),
                |_| async { panic!("forged origin must not reach core") },
            )
            .await
            .unwrap();
        let forged = mod_test_value(&forged);
        assert!(forged["consumed"]
            .as_str()
            .unwrap()
            .contains("origin and event are pinned"));
    }

    #[tokio::test]
    async fn session_compact_runs_the_core_inside_next_and_validates_skip_results() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("compact.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('session.compact', async ($, e, next) => {
                if (e.instructions === 'skip') return { skip: 'keep this conversation' };
                if (e.instructions === 'rewrite') {
                  const result = await next({
                    ...e,
                    instructions: 'rewritten instructions',
                    messages: [...e.messages, { role: 'assistant', text: 'added' }]
                  });
                  return { ...result, tokensAfter: 7 };
                }
                if (e.instructions === 'late-skip') {
                  await next(e);
                  return { skip: 'too late' };
                }
                if (e.instructions === 'bad-usage') {
                  const result = await next(e);
                  return { ...result, usage: { input_tokens: 1 } };
                }
                return next(e);
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("compact", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();

        let input = |instructions: &str| {
            json!({
                "trigger":"manual",
                "instructions":instructions,
                "messages":[{"role":"user","text":"original"}]
            })
        };
        let skipped = host
            .dispatch(
                "session.compact",
                test_projection(input("skip")),
                |_| async { panic!("a skip answer must not run the compact callback") },
            )
            .await
            .unwrap();
        let skipped = mod_test_value(&skipped);
        assert_eq!(skipped, json!({"skip":"keep this conversation"}));

        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_by_core = calls.clone();
        let compacted = json!({
            "messages":[{"role":"assistant","text":"summary"}],
            "tokensBefore":41,
            "tokensAfter":11,
            "usage":{
                "input_tokens":8,
                "output_tokens":3,
                "cache_read_input_tokens":1,
                "cache_creation_input_tokens":0
            }
        });
        let compacted_for_core = compacted.clone();
        let rewritten = host
            .dispatch(
                "session.compact",
                test_projection(input("rewrite")),
                move |event| {
                    let event = mod_test_event(event.clone());
                    let calls = calls_by_core.clone();
                    let compacted = compacted_for_core.clone();
                    async move {
                        calls.lock().unwrap().push(event.clone());
                        Ok(Some(test_projection(compacted)))
                    }
                },
            )
            .await
            .unwrap();
        let rewritten = mod_test_value(&rewritten);
        assert_eq!(rewritten["tokensBefore"], json!(41));
        assert_eq!(rewritten["tokensAfter"], json!(7));
        assert_eq!(
            calls.lock().unwrap()[0]["instructions"],
            json!("rewritten instructions")
        );
        assert_eq!(
            calls.lock().unwrap()[0]["messages"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        for instructions in ["late-skip", "bad-usage"] {
            let core_result = compacted.clone();
            let answer = host
                .dispatch(
                    "session.compact",
                    test_projection(input(instructions)),
                    move |_| {
                        let core_result = core_result.clone();
                        async move { Ok(Some(test_projection(core_result))) }
                    },
                )
                .await
                .unwrap();
            let answer = mod_test_value(&answer);
            assert_eq!(answer, compacted);
        }
    }

    #[tokio::test]
    async fn session_compact_accepts_optional_fractional_token_counts() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("compact-optional-counts.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('session.compact', ($, event, next) => next(event));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "compact-optional-counts",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let input = json!({
            "trigger":"manual",
            "messages":[{"role":"user","text":"original"}]
        });

        let without_counts = json!({
            "messages":[{"role":"assistant","text":"summary"}]
        });
        let answer = host
            .dispatch("session.compact", test_projection(input.clone()), {
                let without_counts = without_counts.clone();
                move |_| {
                    let answer = without_counts.clone();
                    async move { Ok(Some(test_projection(answer))) }
                }
            })
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, without_counts);

        let fractional_counts = json!({
            "messages":[{"role":"assistant","text":"summary"}],
            "tokensBefore": 12.5,
            "tokensAfter": 3.25,
            "usage": {
                "input_tokens": 8.5,
                "output_tokens": 2.25,
                "cache_read_input_tokens": 1,
                "cache_creation_input_tokens": 0.5
            }
        });
        let answer = host
            .dispatch("session.compact", test_projection(input), {
                let fractional_counts = fractional_counts.clone();
                move |_| {
                    let answer = fractional_counts.clone();
                    async move { Ok(Some(test_projection(answer))) }
                }
            })
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer, fractional_counts);
    }

    #[tokio::test]
    async fn ui_resolve_is_cached_at_load_and_above_prompt_renders_box_text() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("ui-render.js");
        std::fs::write(
            &module,
            r#"let resolves = 0;
            let renders = 0;
            export function register(on) {
              on('ui.resolve', { surface: 'terminal', component: 'AbovePrompt' }, ($, e, next) => {
                resolves += 1;
                if (Object.hasOwn(e, 'requestId')) throw new Error('resolve has no requestId');
                const table = $.ui.resolve(e);
                if (typeof table.Box !== 'function' || typeof table.Text !== 'function'
                    || typeof table.Button !== 'function') throw new Error('unexpected supported element table');
                return table;
              });
              on('ui.render', { component: 'AbovePrompt' }, ($, e) => {
                const { Box, Text } = $.ui.resolve(e);
                return Box({ flexDirection: 'column', children: [
                  Text({ children: ['resolve count: ' + resolves] }),
                  Text({ children: ['render count: ' + (++renders)] }),
                  Text({ children: ['body columns: ' + e.props.bodyColumns] }),
                ] });
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "ui-render",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();

        let input = |viewport: Option<Value>| {
            let mut event = json!({
                "surface":"terminal",
                "component":"AbovePrompt",
                "requestId":"above-prompt-main",
                "props":{"hasSurvey":false,"isWorking":false,"maxRows":12,"bodyColumns":80,
                    "scroll":{"offset":0,"bodyRows":11},"view":{}}
            });
            if let Some(viewport) = viewport {
                event["viewport"] = viewport;
            }
            event
        };
        for (expected_render_count, viewport) in [
            (1, None),
            (2, Some(json!({"columns":80,"rows":24,"isFullscreen":true}))),
        ] {
            let answer = host
                .dispatch("ui.render", test_projection(input(viewport)), |_| async {
                    panic!("a render hook that returns a tree must not call core")
                })
                .await
                .unwrap();
            let answer = mod_test_value(&answer);
            assert_eq!(answer["type"], "Box");
            assert_eq!(answer["children"][0]["children"][0], "resolve count: 1");
            assert_eq!(
                answer["children"][1]["children"][0],
                format!("render count: {expected_render_count}")
            );
            assert_eq!(answer["children"][2]["children"][0], "body columns: 80");
        }

        let unsupported = host
            .dispatch(
                "ui.render",
                test_projection(json!({
                    "surface":"terminal",
                    "component":"Pane",
                    "requestId":"some-pane",
                    "props":{}
                })),
                |_| async { Ok(Some(test_projection(Value::Null))) },
            )
            .await;
        assert!(unsupported.is_err());
    }

    #[tokio::test]
    async fn ui_render_invalidation_advances_the_session_generation() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("ui-invalidate.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('ui.render', ($, e) => {
                $.ui.invalidate('ui.render');
                $.ui.invalidate('ui.render');
                const { Text } = $.ui.resolve(e);
                return Text({ children: ['fresh'] });
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "ui-invalidate",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let input = json!({
            "surface":"terminal",
            "component":"AbovePrompt",
            "requestId":"above-prompt",
            "props":{"hasSurvey":false,"isWorking":false,"maxRows":3,"bodyColumns":80,
                "scroll":{"offset":0,"bodyRows":2},"view":{}},
            "viewport":{"columns":80,"rows":24,"isFullscreen":false}
        });
        let answer = host
            .dispatch("ui.render", test_projection(input), |_| async {
                panic!("a UI tree result must not call core")
            })
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["children"][0], "fresh");
        let reached_generation = host.ui_render_generation();
        assert!((1..=2).contains(&reached_generation));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while host.ui_render_generation() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the steady fold emits its queued ui.render invalidation");
        assert_eq!(host.ui_render_generation(), 2);
    }

    #[tokio::test]
    async fn ui_render_live_invalidation_uses_the_terminal_render_context() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("ui-live-invalidate.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('ui.render', ($, event) => {
                $.ui.invalidate('ui.render');
                $.ui.invalidate('ui.render');
                const { Text } = $.ui.resolve(event);
                return Text({ children: ['live'] });
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "ui-live-invalidate",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session_impl = Arc::new(TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test-model".into()),
            id: "live-invalidation-session".into(),
            turns: 1,
        });
        let session: Arc<dyn ModSessionContext> = session_impl;
        host.attach_background_context(Arc::downgrade(&session));
        let outcome = host
            .dispatch_ui_render_with_state_scope_at_session(
                test_projection(json!({
                    "surface":"terminal",
                    "component":"AbovePrompt",
                    "requestId":"above-prompt",
                    "props":{"hasSurvey":false,"isWorking":false,"maxRows":3,
                        "bodyColumns":80,"scroll":{"offset":0,"bodyRows":2},"view":{}},
                    "viewport":{"columns":80,"rows":24,"isFullscreen":false},
                })),
                1,
                session.as_ref(),
                |_| async { panic!("a UI render tree must not call core") },
                |_, _| async {},
                |_, _, _| async {},
                |_, _| async {},
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.result_projection.string_at("/children/0"),
            Some("live")
        );
        let reached_generation = host.ui_render_generation();
        assert!((1..=3).contains(&reached_generation));
        if reached_generation < 3 {
            let state = host.state.lock().unwrap();
            let (steady, live) = &state.ui_render_plugin_pacing["ui-live-invalidate"];
            assert!(steady.pending.is_some());
            assert!(
                live.pending.is_some() || live.last_at_ms > steady.last_at_ms,
                "ui.invalidate reached from the active live render must also use its 34 ms counter"
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while host.ui_render_generation() < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the queued live or steady ui.render invalidation reaches its bump");
    }

    #[derive(Clone)]
    struct GenerationInvalidationSession {
        current: Arc<AtomicBool>,
        emissions: Arc<AtomicU64>,
        cancellation: lingxi_core::host::CancellationToken,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for GenerationInvalidationSession {
        fn cwd(&self) -> PathBuf {
            PathBuf::from("/tmp/generation-invalidation")
        }

        fn root(&self) -> PathBuf {
            self.cwd()
        }

        fn ui_invalidation_context(&self) -> Option<Arc<dyn ModSessionContext>> {
            Some(Arc::new(self.clone()))
        }

        fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
            Some(self.cancellation.clone())
        }

        async fn model(&self) -> String {
            "test-model".into()
        }

        async fn id(&self) -> String {
            "generation-invalidation".into()
        }

        async fn turns(&self) -> u64 {
            0
        }

        async fn emit_mod_ui_invalidate(
            &self,
            _instances_json: Option<&Utf16JsonProjection>,
            _uuid: &str,
            _session_id: &str,
        ) {
            if self.current.load(Ordering::Acquire) {
                self.emissions.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    struct BackgroundInvalidationSession {
        emissions: Arc<AtomicU64>,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for BackgroundInvalidationSession {
        fn cwd(&self) -> PathBuf {
            PathBuf::from("/tmp/background-invalidation")
        }

        fn root(&self) -> PathBuf {
            self.cwd()
        }

        async fn model(&self) -> String {
            "test-model".into()
        }

        async fn id(&self) -> String {
            "background-invalidation".into()
        }

        async fn turns(&self) -> u64 {
            0
        }

        async fn emit_mod_ui_invalidate(
            &self,
            _instances_json: Option<&Utf16JsonProjection>,
            _uuid: &str,
            _session_id: &str,
        ) {
            self.emissions.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[tokio::test]
    async fn cancelled_ticket_rejects_admission_before_async_cleanup() {
        let host = ModHost::start(None).await.unwrap();
        let cancellation = lingxi_core::host::CancellationToken::new();
        let session: Arc<dyn ModSessionContext> = Arc::new(GenerationInvalidationSession {
            current: Arc::new(AtomicBool::new(true)),
            emissions: Arc::new(AtomicU64::new(0)),
            cancellation: cancellation.clone(),
        });
        let ticket = "cancelled-before-cleanup";
        let epoch = host.current_worker_epoch().await;
        let sessions = &epoch.generation_api_sessions;
        sessions.lock().unwrap().insert(
            ticket.into(),
            GenerationApiSessionEntry {
                session,
                references: 1,
            },
        );
        assert!(retain_generation_api_session(&sessions, ticket));
        assert!(host.generation_api_session_in(sessions, ticket).is_ok());
        release_generation_api_session(&sessions, ticket);

        cancellation.cancel();
        // Deliberately leave the map entry present to exercise the interval
        // before the asynchronous invalidation watcher removes it.
        assert!(sessions.lock().unwrap().contains_key(ticket));
        assert!(!retain_generation_api_session(&sessions, ticket));
        assert!(matches!(
            host.generation_api_session_in(sessions, ticket),
            Err(ModError::Unavailable(_))
        ));
        assert_eq!(sessions.lock().unwrap()[ticket].references, 1);
        release_generation_api_session(&sessions, ticket);
        assert!(!sessions.lock().unwrap().contains_key(ticket));
    }

    #[tokio::test]
    async fn worker_timer_state_set_is_rejected_after_generation_reset_at_route_boundary() {
        async fn wait_for_state_value(host: &ModHost, key: &str, expected: &str) {
            let expected =
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(expected).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let value = host
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .values
                        .get(&ModStateKey::test("worker-generation-route-test", key))
                        .map(|entry| entry.value.clone());
                    if value.as_ref() == Some(&expected) {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("worker callback did not set state key {key}"));
        }

        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("generation-route-timer.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.call', ($, e) => {
                const prefix = e.tool === 'StaleSchedule' ? 'stale' : 'fresh';
                $.clock.after(0, async () => {
                  await $.state.set({ plugin: 'worker-generation-route-test', key: `${prefix}Started` }, true);
                  await $.state.set({ plugin: 'worker-generation-route-test', key: `${prefix}Late` }, true);
                });
                return { result: 'scheduled' };
              });
            }
        "#,
        )
        .unwrap();

        let host = ModHost::start(None).await.unwrap();
        let background: Arc<dyn ModSessionContext> = Arc::new(BackgroundInvalidationSession {
            emissions: Arc::new(AtomicU64::new(0)),
        });
        host.attach_background_context(Arc::downgrade(&background));
        host.load(
            "worker-generation-route-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();

        let (late_state_set_entered, release_late_state_set) =
            host.test_route_gate.pause_state_key("staleLate");
        let callback_rejected = host.test_route_gate.expect_expired_callback_error();
        let stale_cancellation = lingxi_core::host::CancellationToken::new();
        let stale_session = GenerationInvalidationSession {
            current: Arc::new(AtomicBool::new(true)),
            emissions: Arc::new(AtomicU64::new(0)),
            cancellation: stale_cancellation.clone(),
        };
        let stale = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"StaleSchedule"})),
                &stale_session,
                |_| async { panic!("the Mod handles timer scheduling") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let stale = mod_test_value(&stale);
        assert_eq!(stale["result"], "scheduled");

        wait_for_state_value(&host, "staleStarted", "true").await;
        let (timer_ticket, route_retained_ticket) =
            tokio::time::timeout(Duration::from_secs(5), late_state_set_entered)
                .await
                .expect("the actual clock.after callback reaches the gated state.set API")
                .expect("the worker route gate remains open");
        assert!(
            !timer_ticket.is_empty(),
            "clock.after retains its generation ticket"
        );
        assert!(
            route_retained_ticket,
            "the worker output route retains the live generation ticket before queueing the API call"
        );

        // The callback's state.set has passed worker-route ticket retention but
        // has not reached background_api_loop. Reset now; releasing the queued
        // request must reject its generation ticket before mutating host state.
        stale_cancellation.cancel();
        release_late_state_set
            .send(())
            .expect("the actual worker route is waiting at the state.set boundary");
        tokio::time::timeout(Duration::from_secs(5), callback_rejected)
            .await
            .expect("the callback receives the expired-generation API rejection")
            .expect("the worker reports the rejected callback");

        let stale_late = host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values
            .get(&ModStateKey::test(
                "worker-generation-route-test",
                "staleLate",
            ))
            .map(|entry| entry.value.clone());
        assert_eq!(
            stale_late, None,
            "expired callback must not mutate state.set"
        );

        // A new generation traverses the same real worker and API route and is
        // admitted normally, ruling out a test that merely disconnected APIs.
        let fresh_session = GenerationInvalidationSession {
            current: Arc::new(AtomicBool::new(true)),
            emissions: Arc::new(AtomicU64::new(0)),
            cancellation: lingxi_core::host::CancellationToken::new(),
        };
        let fresh = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"FreshSchedule"})),
                &fresh_session,
                |_| async { panic!("the Mod handles timer scheduling") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let fresh = mod_test_value(&fresh);
        assert_eq!(fresh["result"], "scheduled");
        wait_for_state_value(&host, "freshLate", "true").await;
    }

    #[tokio::test]
    async fn ui_invalidation_timer_keeps_its_generation_bound_session() {
        let host = ModHost::start(None).await.unwrap();
        let generation_emissions = Arc::new(AtomicU64::new(0));
        let current = Arc::new(AtomicBool::new(true));
        let generation_session = GenerationInvalidationSession {
            current: current.clone(),
            emissions: generation_emissions.clone(),
            cancellation: lingxi_core::host::CancellationToken::new(),
        };
        let background_emissions = Arc::new(AtomicU64::new(0));
        let background: Arc<dyn ModSessionContext> = Arc::new(BackgroundInvalidationSession {
            emissions: background_emissions.clone(),
        });
        host.attach_background_context(Arc::downgrade(&background));

        let site = ModUiRenderSiteKey {
            surface: "terminal".into(),
            component: "AbovePrompt".into(),
            request_id: "generation-bound-site".encode_utf16().collect(),
        };
        host.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ui_render_sites
            .insert(
                site,
                ModUiRenderSiteState {
                    pace: ModUiRenderPace::Live,
                    has_clients: true,
                    render_plugins: vec!["generation-bound-plugin".into()],
                    ..Default::default()
                },
            );

        let immediate = host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request_ui_render_plugin_invalidation("generation-bound-plugin", true, 0);
        host.apply_ui_invalidation_update(immediate, Some(&generation_session))
            .await;
        let before_retire = generation_emissions.load(Ordering::Acquire);
        assert_eq!(
            before_retire, 1,
            "both counter bumps share one invalidation frame"
        );

        let paced = host
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request_ui_render_plugin_invalidation("generation-bound-plugin", true, 1);
        assert_eq!(
            paced.timers.len(),
            2,
            "both current pace counters schedule their next bump"
        );
        host.apply_ui_invalidation_update(paced, Some(&generation_session))
            .await;
        current.store(false, Ordering::Release);

        tokio::time::timeout(Duration::from_secs(1), async {
            while {
                let state = host.state.lock().unwrap();
                let (steady, live) = &state.ui_render_plugin_pacing["generation-bound-plugin"];
                steady.pending.is_some() || live.pending.is_some()
            } {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("the paced invalidation timer fires");
        assert_eq!(generation_emissions.load(Ordering::Acquire), before_retire);
        assert_eq!(
            background_emissions.load(Ordering::Acquire),
            0,
            "a W1-originated timer must not fall back to the host background session"
        );
    }

    #[tokio::test]
    async fn repeated_live_render_uses_worker_evaluation_duration_for_slow_marking() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("ui-render-duration.js");
        std::fs::write(
            &module,
            r#"let evaluationCount = 0;
            export function register(on) {
              on('ui.render', { surface: 'terminal', component: 'AbovePrompt' }, async ($, event) => {
                evaluationCount += 1;
                await $.clock.sleep(40);
                const { Text } = $.ui.resolve(event);
                return Text({ children: ['evaluation:' + evaluationCount] });
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "slow-live",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let session_impl = Arc::new(TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test-model".into()),
            id: "slow-live-session".into(),
            turns: 1,
        });
        let session: Arc<dyn ModSessionContext> = session_impl;
        host.attach_background_context(Arc::downgrade(&session));
        let render = |scroll_offset| {
            json!({
                "surface":"terminal",
                "component":"AbovePrompt",
                "requestId":"slow-live-site",
                "props":{"hasSurvey":false,"isWorking":false,"maxRows":3,
                    "bodyColumns":80,"scroll":{"offset":scroll_offset,"bodyRows":2},"view":{}},
                "viewport":{"columns":80,"rows":24,"isFullscreen":false},
            })
        };
        let core = |event| async move { Ok(event) };
        let on_log = |_: String, _: Utf16JsonProjection| async {};
        let on_toast = |_: String, _: Utf16JsonProjection, _: u64| async {};
        let on_status = |_: String, _: Option<Utf16JsonProjection>| async {};
        let first = host
            .dispatch_ui_render_with_state_scope_at_session(
                test_projection(render(0)),
                1,
                session.as_ref(),
                core,
                on_log,
                on_toast,
                on_status,
            )
            .await
            .unwrap();
        let first_result = mod_test_value(&first.result_projection);
        assert_eq!(first_result["children"][0], "evaluation:1");
        assert!(first.ui_render_duration_ms.is_some());
        assert!(
            host.state
                .lock()
                .unwrap()
                .ui_render_slow_plugins
                .get("slow-live")
                .is_none(),
            "Native does not mark the first uncached evaluation slow"
        );

        // Keep the Native site identity so this is a repeated live evaluation,
        // but change a validated prop so the render signature cannot reuse the
        // previous result without evaluating the hook again.
        let second = host
            .dispatch_ui_render_with_state_scope_at_session(
                test_projection(render(1)),
                2,
                session.as_ref(),
                |event| async move { Ok(event) },
                |_: String, _: Utf16JsonProjection| async {},
                |_: String, _: Utf16JsonProjection, _: u64| async {},
                |_: String, _: Option<Utf16JsonProjection>| async {},
            )
            .await
            .unwrap();
        let second_result = mod_test_value(&second.result_projection);
        assert_eq!(second_result["children"][0], "evaluation:2");
        let measured_ms = second
            .ui_render_duration_ms
            .expect("worker result carries the ui.render evaluator duration");
        assert!(
            measured_ms >= 34,
            "the worker-side Date.now interval should include the delayed evaluator: {measured_ms} ms"
        );
        let slow_until = host
            .state
            .lock()
            .unwrap()
            .ui_render_slow_plugins
            .get("slow-live")
            .copied()
            .expect("a slow repeated live evaluator marks its participating plugin");
        assert!(slow_until > host.ui_pacing_now_ms());
    }

    #[test]
    fn ui_render_plugin_invalidations_use_native_steady_and_live_pacing() {
        let mut state = ModStateStore::default();
        let steady_site = ModUiRenderSiteKey {
            surface: "desktop".into(),
            component: "AbovePrompt".into(),
            request_id: "steady-site".encode_utf16().collect(),
        };
        let live_site = ModUiRenderSiteKey {
            surface: "terminal".into(),
            component: "PromptHint".into(),
            request_id: "live-site".encode_utf16().collect(),
        };
        for (key, pace) in [
            (&steady_site, ModUiRenderPace::Steady),
            (&live_site, ModUiRenderPace::Live),
        ] {
            let mut site = ModUiRenderSiteState::default();
            site.pace = pace;
            site.render_plugins.push("paint".into());
            state.ui_render_sites.insert(key.clone(), site);
        }

        let first = state.request_ui_render_plugin_invalidation("paint", true, 0);
        assert!(first.version_changed);
        assert!(first.timers.is_empty());
        assert_eq!(first.sites.len(), 2);
        assert_eq!(state.ui_render_sites[&steady_site].token, 1);
        assert_eq!(state.ui_render_sites[&live_site].token, 1);

        let second = state.request_ui_render_plugin_invalidation("paint", true, 10);
        assert!(!second.version_changed);
        assert!(second.sites.is_empty());
        assert_eq!(second.timers.len(), 2);
        let live_timer = second
            .timers
            .iter()
            .find(|timer| {
                matches!(
                    &timer.kind,
                    ModUiPacingTimerKind::Plugin { plugin, pace }
                        if plugin == "paint" && *pace == ModUiRenderPace::Live
                )
            })
            .unwrap();
        let steady_timer = second
            .timers
            .iter()
            .find(|timer| {
                matches!(
                    &timer.kind,
                    ModUiPacingTimerKind::Plugin { plugin, pace }
                        if plugin == "paint" && *pace == ModUiRenderPace::Steady
                )
            })
            .unwrap();
        assert_eq!(live_timer.deadline_ms, 34);
        assert_eq!(steady_timer.deadline_ms, 100);

        let live_bump = state.fire_ui_render_pacing_timer(live_timer, 34);
        assert_eq!(live_bump.sites, vec![live_site.clone()]);
        assert_eq!(state.ui_render_sites[&steady_site].token, 1);
        assert_eq!(state.ui_render_sites[&live_site].token, 2);

        let steady_bump = state.fire_ui_render_pacing_timer(steady_timer, 100);
        assert!(steady_bump.sites.contains(&steady_site));
        assert!(steady_bump.sites.contains(&live_site));
        assert_eq!(state.ui_render_sites[&steady_site].token, 2);
        assert_eq!(state.ui_render_sites[&live_site].token, 3);
    }

    #[test]
    fn state_set_flushes_pending_steady_and_live_reader_folds() {
        let mut state = ModStateStore::default();
        let key = ModStateKey::test("reader", "screen");
        let steady_site = ModUiRenderSiteKey {
            surface: "desktop".into(),
            component: "ToolResult".into(),
            request_id: "steady-reader".encode_utf16().collect(),
        };
        let live_site = ModUiRenderSiteKey {
            surface: "terminal".into(),
            component: "PromptHint".into(),
            request_id: "live-reader".encode_utf16().collect(),
        };
        for (site_key, pace) in [
            (&steady_site, ModUiRenderPace::Steady),
            (&live_site, ModUiRenderPace::Live),
        ] {
            let mut site = ModUiRenderSiteState::default();
            site.pace = pace;
            site.reads.insert(key.clone());
            state.ui_render_sites.insert(site_key.clone(), site);
        }
        state.ui_render_state_folds.0.last_at_ms = Some(0);
        state.ui_render_state_folds.1.last_at_ms = Some(0);

        let queued = state.schedule_ui_render_write(&key, 10);
        assert_eq!(queued.timers.len(), 2);
        assert_eq!(
            queued
                .timers
                .iter()
                .map(|timer| timer.deadline_ms)
                .collect::<HashSet<_>>(),
            HashSet::from([34, 100])
        );

        // The state.set bottom calls both fold flushes before its API reply.
        let flushed = state.flush_ui_render_state_folds(20);
        assert_eq!(flushed.sites.len(), 2);
        assert_eq!(state.ui_render_sites[&steady_site].token, 1);
        assert_eq!(state.ui_render_sites[&live_site].token, 1);

        for timer in &queued.timers {
            let stale = state.fire_ui_render_pacing_timer(timer, timer.deadline_ms);
            assert!(stale.sites.is_empty());
        }
        assert_eq!(state.ui_render_sites[&steady_site].token, 1);
        assert_eq!(state.ui_render_sites[&live_site].token, 1);
    }

    #[test]
    fn slow_ui_render_plugin_routes_bursts_and_state_readers_to_steady_pace() {
        let mut state = ModStateStore::default();
        let live_site = ModUiRenderSiteKey {
            surface: "terminal".into(),
            component: "AbovePrompt".into(),
            request_id: "live-slow-reader".encode_utf16().collect(),
        };
        let steady_site = ModUiRenderSiteKey {
            surface: "desktop".into(),
            component: "ToolResult".into(),
            request_id: "steady-slow-reader".encode_utf16().collect(),
        };
        let key = ModStateKey::test("slow-paint", "screen");
        let mut site = ModUiRenderSiteState::default();
        site.pace = ModUiRenderPace::Live;
        site.render_plugins.push("slow-paint".into());
        site.reads.insert(key.clone());
        state.ui_render_sites.insert(live_site.clone(), site);
        let mut site = ModUiRenderSiteState::default();
        site.pace = ModUiRenderPace::Steady;
        site.render_plugins.push("slow-paint".into());
        site.reads.insert(key.clone());
        state.ui_render_sites.insert(steady_site, site);

        // Prime both pacing paths through production transitions: the first
        // Native bS call bumps immediately, then a read-set change flushes the
        // initially unthrottled steady/live folds.
        let initial_bump = state.request_ui_render_plugin_invalidation("slow-paint", true, 0);
        assert!(initial_bump.version_changed);
        assert!(initial_bump.timers.is_empty());
        assert_eq!(initial_bump.sites.len(), 2);
        let initial_read = state.schedule_ui_render_write(&key, 0);
        assert!(initial_read.timers.is_empty());
        assert_eq!(initial_read.sites.len(), 2);
        state.mark_ui_render_plugins_slow(["slow-paint"].into_iter(), 0);

        let burst = state.request_ui_render_plugin_invalidation("slow-paint", true, 10);
        assert_eq!(burst.timers.len(), 1);
        assert!(matches!(
            &burst.timers[0].kind,
            ModUiPacingTimerKind::Plugin {
                pace,
                ..
            } if *pace == ModUiRenderPace::Steady
        ));
        assert_eq!(burst.timers[0].deadline_ms, 100);

        let reader = state.schedule_ui_render_write(&key, 10);
        assert_eq!(reader.timers.len(), 1);
        assert!(matches!(
            &reader.timers[0].kind,
            ModUiPacingTimerKind::StateFold(pace) if *pace == ModUiRenderPace::Steady
        ));
        assert_eq!(reader.timers[0].deadline_ms, 100);
    }

    #[tokio::test]
    async fn ui_button_press_reaches_matching_callback_and_rejects_old_worker_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("ui-button.js");
        std::fs::write(
            &module,
            r#"let presses = 0;
            export function register(on) {
              on('ui.press', ($, e, next) => {
                if (Object.keys(e).sort().join(',') !== 'component,element,plugin,requestId,surface') {
                  throw new Error('private callback identity leaked into ui.press');
                }
                return next(e);
              });
              on('ui.render', { surface: 'terminal', component: 'AbovePrompt' }, ($, e) => {
                const { Box, Button, Text } = $.ui.resolve(e);
                return Box({ flexDirection: 'column', children: [
                  Text({ children: ['presses: ' + presses] }),
                  Button({ key: 'refresh', hotkey: '1', children: [
                    'A', Text({ bold: true, children: ['B', 'C'] }),
                  ],
                    onPress: () => { presses += 1; } }),
                  Button({ key: 'folded', children: 'Folded', onPress: () => {} }),
                ] });
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load_with_tier_order_storage(
            "button-mod",
            "button-storage",
            dir.path(),
            &module,
            test_mod_options(json!({})),
            "user",
            None,
        )
        .await
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test-model".into()),
            id: "button-session".into(),
            turns: 0,
        };
        fn render_input() -> Utf16JsonProjection {
            mod_test_projection(
                r#"{
                "surface":"terminal",
                "component":"AbovePrompt",
                "requestId":"above-prompt",
                "props":{"hasSurvey":false,"isWorking":false,"maxRows":3,"bodyColumns":80,
                    "scroll":{"offset":0,"bodyRows":2},"view":{}},
                "viewport":{"columns":80,"rows":24,"isFullscreen":false}
            }"#,
            )
        }
        async fn render(
            host: &ModHost,
            session: &TestSessionContext,
            revision: u64,
        ) -> Utf16JsonProjection {
            let input = render_input();
            let registration_revision = host
                .begin_mod_ui_render(&session.id, &input, revision)
                .unwrap();
            let tree = host
                .dispatch("ui.render", input.clone(), |_| async {
                    panic!("Button render must not ask core for a tree")
                })
                .await
                .unwrap();
            host.finish_mod_ui_render(&session.id, &input, revision, registration_revision, &tree)
                .unwrap();
            tree
        }
        let press_input = |tree: &Utf16JsonProjection| {
            let mut input = mod_test_projection(
                r#"{"surface":"terminal","component":"AbovePrompt","requestId":"above-prompt"}"#,
            );
            set_projection_field(
                &mut input,
                "plugin",
                tree.subprojection("/children/1/press/plugin").unwrap(),
            )
            .unwrap();
            set_projection_field(
                &mut input,
                "element",
                tree.subprojection("/children/1/props/key").unwrap(),
            )
            .unwrap();
            set_projection_field(
                &mut input,
                "press",
                object_projection(vec![
                    (
                        "handle".into(),
                        tree.subprojection("/children/1/press/handle").unwrap(),
                    ),
                    (
                        "workerEpoch".into(),
                        tree.subprojection("/children/1/press/workerEpoch").unwrap(),
                    ),
                    (
                        "renderRevision".into(),
                        tree.subprojection("/children/1/press/renderRevision")
                            .unwrap(),
                    ),
                ])
                .unwrap(),
            )
            .unwrap();
            input
        };
        let no_log = |_: String, _: Utf16JsonProjection| async {};
        let no_toast = |_: String, _: Utf16JsonProjection, _: u64| async {};
        let no_status = |_: String, _: Option<Utf16JsonProjection>| async {};

        let first = render(&host, &session, 1).await;
        assert_eq!(mod_test_value(&first)["children"][1]["press"]["handle"], 1);
        let first_value = mod_test_value(&first);
        assert_eq!(first_value["children"][1]["props"]["label"], "ABC");
        assert_eq!(first_value["children"][1]["children"][0], "A");
        assert_eq!(first_value["children"][1]["children"][1]["type"], "Text");
        assert_eq!(
            first_value["children"][1]["children"][1]["props"]["bold"],
            true
        );
        assert_eq!(
            first_value["children"][1]["children"][1]["children"],
            json!(["B", "C"])
        );
        assert_eq!(first_value["children"][2]["props"]["label"], "Folded");
        assert!(first_value["children"][2].get("children").is_none());
        let first_press = press_input(&first);
        assert_eq!(
            mod_test_value(
                &host
                    .dispatch_ui_press_at_session(
                        first_press.clone(),
                        1,
                        &session,
                        no_log,
                        no_toast,
                        no_status,
                    )
                    .await
                    .unwrap()
            )["handled"],
            true
        );
        let second = render(&host, &session, 2).await;
        assert_eq!(
            mod_test_value(&second)["children"][0]["children"][0],
            "presses: 1"
        );
        let stale_from_redraw = host
            .dispatch_ui_press_at_session(
                first_press.clone(),
                2,
                &session,
                no_log,
                no_toast,
                no_status,
            )
            .await
            .unwrap();
        assert_eq!(mod_test_value(&stale_from_redraw), json!({"handled":false}));
        let second_press = press_input(&second);
        assert_eq!(
            mod_test_value(
                &host
                    .dispatch_ui_press_at_session(
                        second_press,
                        2,
                        &session,
                        no_log,
                        no_toast,
                        no_status,
                    )
                    .await
                    .unwrap()
            )["handled"],
            true
        );

        host.unload("button-storage").await.unwrap();
        host.load_with_tier_order_storage(
            "button-mod",
            "button-storage",
            dir.path(),
            &module,
            test_mod_options(json!({})),
            "user",
            None,
        )
        .await
        .unwrap();
        let third = render(&host, &session, 3).await;
        let third_value = mod_test_value(&third);
        let first_value = mod_test_value(&first);
        assert_eq!(third_value["children"][0]["children"][0], "presses: 0");
        assert_eq!(third_value["children"][1]["press"]["handle"], 1);
        assert_ne!(
            third_value["children"][1]["press"]["workerEpoch"],
            first_value["children"][1]["press"]["workerEpoch"],
            "worker reload may reuse a numeric handle only under a new epoch"
        );
        assert_eq!(
            mod_test_value(
                &host
                    .dispatch_ui_press_at_session(
                        first_press,
                        3,
                        &session,
                        no_log,
                        no_toast,
                        no_status,
                    )
                    .await
                    .unwrap()
            ),
            json!({"handled":false})
        );
        let current_press = press_input(&third);
        assert_eq!(
            mod_test_value(
                &host
                    .dispatch_ui_press_at_session(
                        current_press,
                        3,
                        &session,
                        no_log,
                        no_toast,
                        no_status,
                    )
                    .await
                    .unwrap()
            )["handled"],
            true
        );
        let fourth = render(&host, &session, 4).await;
        assert_eq!(
            mod_test_value(&fourth)["children"][0]["children"][0],
            "presses: 1"
        );
    }

    #[tokio::test]
    async fn ui_press_site_revision_rejects_out_of_order_render_and_clear() {
        let host = ModHost::start(None).await.unwrap();
        let session_id = "revision-session";
        let input = mod_test_projection(
            r#"{
            "surface":"terminal",
            "component":"AbovePrompt",
            "requestId":"above-prompt",
            "props":{"hasSurvey":false,"isWorking":false,"maxRows":3,"bodyColumns":80,
                "scroll":{"offset":0,"bodyRows":2},"view":{}},
            "viewport":{"columns":80,"rows":24,"isFullscreen":false}
        }"#,
        );
        let tree = mod_test_projection(
            r#"{
            "type":"Button",
            "props":{"key":"refresh","label":"Refresh"},
            "children":[],
            "press":{"plugin":"button-mod","handle":1,"workerEpoch":"worker-a","renderRevision":1}
        }"#,
        );
        let registration_revision = host.begin_mod_ui_render(session_id, &input, 5).unwrap();
        host.clear_mod_ui_press_actions(session_id, 6).unwrap();
        host.finish_mod_ui_render(&session_id, &input, 5, registration_revision, &tree)
            .unwrap();
        let site = ModUiPressSite {
            session_id: session_id.into(),
            surface: "terminal".into(),
            component: "AbovePrompt".into(),
            request_id: "above-prompt".encode_utf16().collect(),
        };
        {
            let registry = host.ui_press_registry.lock().unwrap();
            assert_eq!(registry.revisions.get(&site), Some(&6));
            assert!(
                registry.targets.is_empty(),
                "late render must not restore a cleared token"
            );
        }

        let registration_revision = host.begin_mod_ui_render(session_id, &input, 7).unwrap();
        host.finish_mod_ui_render(&session_id, &input, 7, registration_revision, &tree)
            .unwrap();
        host.clear_mod_ui_press_actions(session_id, 6)
            .expect("older clear should be ignored");
        let registry = host.ui_press_registry.lock().unwrap();
        assert_eq!(registry.revisions.get(&site), Some(&7));
        assert_eq!(
            registry.targets.len(),
            1,
            "late clear must not erase the current tree"
        );
        drop(registry);

        // The TUI serializes a clear before render under the same revision.
        // That render is allowed to register its new callback afterward.
        host.clear_mod_ui_press_actions(session_id, 8).unwrap();
        let registration_revision = host.begin_mod_ui_render(session_id, &input, 8).unwrap();
        host.finish_mod_ui_render(&session_id, &input, 8, registration_revision, &tree)
            .unwrap();
        {
            let registry = host.ui_press_registry.lock().unwrap();
            assert_eq!(registry.revisions.get(&site), Some(&8));
            assert_eq!(
                registry.targets.len(),
                1,
                "clear then render accepts the new callback"
            );
        }

        // The opposite order at one revision revokes that render's callback.
        let registration_revision = host.begin_mod_ui_render(session_id, &input, 9).unwrap();
        host.finish_mod_ui_render(&session_id, &input, 9, registration_revision, &tree)
            .unwrap();
        host.clear_mod_ui_press_actions(session_id, 9).unwrap();
        {
            let registry = host.ui_press_registry.lock().unwrap();
            assert_eq!(registry.revisions.get(&site), Some(&9));
            assert!(
                registry.targets.is_empty(),
                "render then same-revision clear revokes its action"
            );
        }

        // A late clear from the prior render cannot erase a newer callback.
        let registration_revision = host.begin_mod_ui_render(session_id, &input, 10).unwrap();
        host.finish_mod_ui_render(&session_id, &input, 10, registration_revision, &tree)
            .unwrap();
        host.clear_mod_ui_press_actions(session_id, 9).unwrap();
        let registry = host.ui_press_registry.lock().unwrap();
        assert_eq!(registry.revisions.get(&site), Some(&10));
        assert_eq!(
            registry.targets.len(),
            1,
            "older clear cannot erase a later callback"
        );
    }

    #[tokio::test]
    async fn session_measure_and_agent_offer_use_their_narrow_event_contracts() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("measure-offer.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('agent.offer', async ($, e, next) => {
                await next({ agent: e.agent, description: e.description, source: e.source });
                return { isOffered: false };
              });
              on('session.measure', async ($, e, next) => next(e));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "measure-offer",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();

        let offered = host
            .dispatch(
                "agent.offer",test_projection(json!({"agent":"reviewer","description":"Review changes","source":"plugin","provider":{"name":"local"}})),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    assert_eq!(event["provider"]["name"], "local");
                    Ok(Some(test_projection(json!({"isOffered":true}))))
                },
            )
            .await
            .unwrap();
        let offered = mod_test_value(&offered);
        assert_eq!(offered, json!({"isOffered":false}));

        let measured = host
            .dispatch(
                "session.measure",
                test_projection(
                    json!({"context":{"model":"test"},"rateLimits":{"remaining":0.5},
                    "cost":{"usd":0.01},"changed":["cost"]}),
                ),
                |_| async { Ok(Some(test_projection(json!({"changed":["cost"]})))) },
            )
            .await
            .unwrap();
        let measured = mod_test_value(&measured);
        assert_eq!(measured, json!({"changed":["cost"]}));

        let extra_agent_field = host
            .dispatch(
                "agent.offer",
                test_projection(
                    json!({"agent":"reviewer","description":"Review changes","source":"plugin",
                    "provider":{"name":"local"},"unknown":true}),
                ),
                |_| async { Ok(Some(test_projection(json!({"isOffered":true})))) },
            )
            .await;
        assert!(extra_agent_field.is_err());
        let extra_measure_field = host
            .dispatch(
                "session.measure",
                test_projection(json!({"context":{},"rateLimits":{},"changed":[],"unknown":true})),
                |_| async { Ok(Some(test_projection(json!({"changed":[]})))) },
            )
            .await;
        assert!(extra_measure_field.is_err());
    }

    #[tokio::test]
    async fn session_append_accepts_cross_realm_spreads_and_keeps_identity_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("append-cross-realm.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('session.append', ($, e, next) => {
                if (e.uuid === 'class') {
                  class Block { constructor() { this.type = 'text'; this.text = 'class'; } }
                  return next({ ...e, message: { ...e.message, content: [new Block()] } });
                }
                if (e.uuid === 'function') {
                  return next({ ...e, message: { ...e.message,
                    content: [{ type: 'text', text: 'function', callback: () => 'bad' }] } });
                }
                if (e.uuid === 'cycle') {
                  const cycle = {};
                  cycle.self = cycle;
                  return next({ ...e, message: { ...e.message, content: [cycle] } });
                }
                if (e.uuid === 'deep') {
                  let deep = { leaf: 'x' };
                  for (let index = 0; index < 34; index++) deep = { child: deep };
                  return next({ ...e, message: { ...e.message, content: [deep] } });
                }
                if (e.uuid === 'identity') return next({ ...e, uuid: 'forged' });
                return next({ ...e, message: { ...e.message,
                  content: e.message.content.map(block => ({ ...block, text: 'new' })) } });
              });
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "append-cross-realm",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();

        let input_for = |uuid: &str| {
            json!({
                "message": {
                    "type":"assistant",
                    "name":"assistant",
                    "role":"assistant",
                    "isMeta":false,
                    "content":[{"type":"text","text":"old"}]
                },
                "door":"assistant",
                "origin":{"kind":"core"},
                "uuid":uuid,
                "agentId":"agent-a"
            })
        };
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let core_seen = seen.clone();
        let rewritten = host
            .dispatch(
                "session.append",
                test_projection(input_for("row-uuid")),
                move |event| {
                    let event = mod_test_event(event.clone());
                    let core_seen = core_seen.clone();
                    async move {
                        core_seen.lock().unwrap().push(event.clone());
                        Ok(Some(test_projection(
                            json!({"message":event["message"],"uuid":event["uuid"]}),
                        )))
                    }
                },
            )
            .await
            .unwrap();
        let rewritten = mod_test_value(&rewritten);
        assert_eq!(
            rewritten,
            json!({"uuid":"row-uuid","message":{
                "type":"assistant","name":"assistant","role":"assistant","isMeta":false,
                "content":[{"type":"text","text":"new"}]}})
        );
        assert_eq!(
            seen.lock().unwrap()[0],
            json!({
                "message":{"type":"assistant","name":"assistant","role":"assistant",
                    "isMeta":false,"content":[{"type":"text","text":"new"}]},
                "door":"assistant","origin":{"kind":"core"},"uuid":"row-uuid","agentId":"agent-a"
            })
        );

        for uuid in ["class", "function", "cycle", "deep", "identity"] {
            let expected = input_for(uuid);
            let core_seen = seen.clone();
            let result = host
                .dispatch(
                    "session.append",
                    test_projection(expected.clone()),
                    move |event| {
                        let event = mod_test_event(event.clone());
                        let core_seen = core_seen.clone();
                        async move {
                            core_seen.lock().unwrap().push(event.clone());
                            Ok(Some(test_projection(
                                json!({"message":event["message"],"uuid":event["uuid"]}),
                            )))
                        }
                    },
                )
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result["uuid"], expected["uuid"]);
            assert_eq!(result["message"], expected["message"]);
            assert_eq!(seen.lock().unwrap().last(), Some(&expected));
        }
    }

    #[tokio::test]
    async fn session_attach_detach_pin_surface_client_and_optional_viewport() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("surface-lifecycle.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('session.attach', async ($, e, next) => {
                if (e.clientId === 'without-viewport') {
                  await next({ ...e, viewport: { columns: 80, rows: 24, isFullscreen: false } })
                    .catch(() => undefined);
                }
                if (e.clientId === 'with-viewport') {
                  await next({ ...e, viewport: { ...e.viewport, rows: e.viewport.rows + 1 } })
                    .catch(() => undefined);
                }
                return next(e);
              });
              on('session.detach', ($, e, next) =>
                e.clientId === 'tamper' ? next({ ...e, reason: 'end' }) : next(e));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "surface-lifecycle",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();

        let seen_attaches = Arc::new(std::sync::Mutex::new(Vec::new()));
        for (input, expected) in [
            (
                json!({"surface":"desktop","clientId":"without-viewport"}),
                json!({"surface":"desktop","clientId":"without-viewport"}),
            ),
            (
                json!({"surface":"mobile","clientId":"with-viewport",
                    "viewport":{"columns":90,"rows":30,"isFullscreen":false}}),
                json!({"surface":"mobile","clientId":"with-viewport",
                    "viewport":{"columns":90,"rows":30,"isFullscreen":false}}),
            ),
        ] {
            let seen = seen_attaches.clone();
            let result = host
                .dispatch("session.attach", test_projection(input), move |event| {
                    let event = mod_test_event(event.clone());
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(event.clone());
                        Ok(Some(test_projection(json!({"clientId":event["clientId"]}))))
                    }
                })
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result["clientId"], expected["clientId"]);
        }
        assert_eq!(
            *seen_attaches.lock().unwrap(),
            vec![
                json!({"surface":"desktop","clientId":"without-viewport"}),
                json!({"surface":"mobile","clientId":"with-viewport",
                    "viewport":{"columns":90,"rows":30,"isFullscreen":false}}),
            ]
        );

        let detached = host
            .dispatch(
                "session.detach",
                test_projection(
                    json!({"surface":"desktop","clientId":"window-a","reason":"detach"}),
                ),
                |event| async move {
                    let event = mod_test_event(event.clone());
                    Ok(Some(test_projection(json!({"clientId":event["clientId"]}))))
                },
            )
            .await
            .unwrap();
        let detached = mod_test_value(&detached);
        assert_eq!(detached, json!({"clientId":"window-a"}));

        let seen_detaches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = seen_detaches.clone();
        let tampered_detach = host
            .dispatch(
                "session.detach",
                test_projection(json!({"surface":"desktop","clientId":"tamper","reason":"detach"})),
                move |event| {
                    let event = mod_test_event(event.clone());
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(event.clone());
                        Ok(Some(test_projection(json!({"clientId":event["clientId"]}))))
                    }
                },
            )
            .await
            .unwrap();
        let tampered_detach = mod_test_value(&tampered_detach);
        assert_eq!(tampered_detach, json!({"clientId":"tamper"}));
        assert_eq!(
            *seen_detaches.lock().unwrap(),
            vec![json!({"surface":"desktop","clientId":"tamper","reason":"detach"})],
            "a rejected forwarded event falls back to the original core input"
        );

        for invalid in [
            json!({"surface":"terminal","clientId":"host"}),
            json!({"surface":"desktop","clientId":"bad/client"}),
            json!({"surface":"desktop","clientId":"window-a","unknown":true}),
        ] {
            assert!(host
                .dispatch("session.attach", test_projection(invalid), |_| async {
                    Ok(Some(test_projection(json!({"clientId":"window-a"}))))
                })
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn session_receive_api_uses_its_echo_core_without_a_host_queue() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("receive-api.js");
        std::fs::write(
            &module,
            r#"export function register(on) {
              on('session.start', async ($, e, next) => {
                const received = await $.session.receive({ origin: { kind: 'peer' }, text: 'raw' });
                const invalid = await $.session.receive({ origin: { kind: 'engine' }, text: 'no' })
                  .catch(error => error.message);
                return { ...await next(e), received, invalid };
              });
              on('session.receive', { origin: 'peer' }, ($, e, next) =>
                next({ ...e, text: 'screened' }));
            }"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "receive-api",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let answer = host
            .dispatch(
                "session.start",
                test_projection(json!({"cwd":"/tmp"})),
                |event| async move { Ok(event) },
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(answer["received"], json!({"text":"screened"}));
        assert!(answer["invalid"]
            .as_str()
            .unwrap()
            .contains("origin.kind is unsupported"));
    }

    #[tokio::test]
    async fn prompt_submit_hook_rewrites_text_and_context_but_pins_origin_and_wait() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("submit-hook.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('prompt.submit', async ($, e, next) => {
                if (e.text === 'rewrite') return next({ ...e, text: 'rewritten', context: ['private note'] });
                if (e.text === 'drop') return { drop: 'refused' };
                if (e.text === 'bad-context') return { text: 'wrong', context: [''] };
                if (e.text === 'pin-wait') {
                  const error = await next({ ...e, wait: true }).catch(error => error.message);
                  return { drop: error };
                }
                if (e.text === 'pin-origin') {
                  const error = await next({ ...e, origin: { kind: 'plugin', name: 'forged' } }).catch(error => error.message);
                  return { drop: error };
                }
                return next(e);
              });
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "submit-hook",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let core_inputs = Arc::new(std::sync::Mutex::new(Vec::new()));
        for (text, expected) in [
            (
                "rewrite",
                json!({"text":"rewritten","context":["private note"],"origin":{"kind":"composer"}}),
            ),
            ("drop", json!({"drop":"refused"})),
            (
                "bad-context",
                json!({"text":"bad-context","context":null,"origin":{"kind":"composer"}}),
            ),
        ] {
            let seen = core_inputs.clone();
            let result = host
                .dispatch("prompt.submit",test_projection(json!({"text":text,"wait":false,"origin":{"kind":"composer"}})), move |event| {
                    let event = mod_test_event(event.clone());
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(event.clone());
                        Ok(Some(test_projection(json!({"text":event["text"],"context":event["context"],"origin":event["origin"]}))))
                    }
                })
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert_eq!(result, expected);
        }
        for text in ["pin-wait", "pin-origin"] {
            let seen = core_inputs.clone();
            let result = host
                .dispatch(
                    "prompt.submit",
                    test_projection(json!({"text":text,"wait":false,"origin":{"kind":"composer"}})),
                    move |event| {
                        let event = mod_test_event(event.clone());
                        let seen = seen.clone();
                        async move {
                            seen.lock().unwrap().push(event.clone());
                            Ok(Some(test_projection(json!({"text":event["text"]}))))
                        }
                    },
                )
                .await
                .unwrap();
            let result = mod_test_value(&result);
            assert!(result["drop"]
                .as_str()
                .unwrap()
                .contains("origin and wait are pinned"));
        }
        let inputs = core_inputs.lock().unwrap();
        assert_eq!(
            inputs.len(),
            2,
            "drop and invalid forwards never reach core"
        );
        assert_eq!(inputs[0]["text"], "rewritten");
        assert_eq!(inputs[0]["context"], json!(["private note"]));
    }

    #[tokio::test]
    async fn command_list_reads_the_live_session_through_an_interceptable_event() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("commands.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('command.list', async ($, e, next) => {
                const result = await next(e);
                return { value: [...result.value, { name: 'extra', description: 'Added', source: 'plugin', plugin: 'commands' }] };
              });
              on('tool.call', async ($) => ({ result: await $.command.list() }));
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("commands", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "commands".into(),
            turns: 0,
        };
        let answer = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let answer = mod_test_value(&answer);
        assert_eq!(
            answer["result"],
            json!([
                {"name":"help","description":"Show help","source":"builtin"},
                {"name":"extra","description":"Added","source":"plugin","plugin":"commands"}
            ])
        );
    }

    #[tokio::test]
    async fn command_register_and_run_dispatch_with_pinned_identity() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("commands.js");
        std::fs::write(&module, r#"
            export function register(on) {
              on('command.register', async ($, e, next) => next(e));
              on('session.start', async ($, e) => {
                try { return { ...e, queued: await $.command.run({ command: 'hello', args: 'Ada' }) }; }
                catch (error) { return { ...e, queuedError: String(error.message) }; }
              });
              on('tool.call', { tool: 'Read' }, async ($) => ({ result: await $.command.register({ name: 'hello', description: 'Greeting' }) }));
              on('tool.call', { tool: 'Write' }, async ($) => {
                let error;
                try { await $.command.register({ name: 'blank', description: ' \t ' }); }
                catch (cause) { error = cause.message; }
                let held;
                try { await $.command.run({ command: 'hello' }); }
                catch (cause) { held = cause.message; }
                const registered = await $.command.register({ name: 'later', description: 'Later', immediate: false, ignored: true });
                return { result: { error, held, registered } };
              });
              on('command.run', { command: 'hello' }, async ($, e, next) => {
                let held = false;
                try { await $.command.run({ command: 'hello' }); }
                catch (error) {
                  if (!String(error.message).includes('called from a command.run hook')) throw error;
                  held = true;
                }
                if (!held) throw new Error('command.run self-wait was accepted');
                try { await next({ ...e, command: 'other' }); }
                catch (error) { if (!String(error.message).includes('pinned')) throw error; }
                const core = await next({ ...e, args: e.args + '!' });
                return { text: core.text + ' served' };
              });
            }
        "#).unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("commands", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "commands".into(),
            turns: 0,
        };
        let registered = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let registered = mod_test_value(&registered);
        assert_eq!(registered["result"], json!({"command":"hello"}));
        let checked = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Write","input":{}})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let checked = mod_test_value(&checked);
        let validation = &checked["result"];
        assert_eq!(
            validation["error"],
            "commands: $.command.register: blank needs a description (what the menu shows)"
        );
        assert_eq!(validation["registered"], json!({"command":"later"}));
        assert!(validation["held"].as_str().unwrap().contains(
            "called from a tool.call hook, it would wait on the turn this hook is holding"
        ));
        let startup = host
            .dispatch_with_log_at_session(
                "session.start",
                test_projection(json!({"cwd":dir.path().to_string_lossy()})),
                &session,
                |event| async move { Ok(event) },
                |_, _| async {},
            )
            .await
            .unwrap();
        let startup = mod_test_value(&startup);
        assert_eq!(
            startup["queued"],
            json!({"text":"commands:hello:Ada"}),
            "{startup}"
        );
        let input = json!({"command":"hello","args":"Ada","origin":{"kind":"composer"},
            "presentation":{"isFullscreen":false,"columns":80}});
        let run = host
            .dispatch("command.run", test_projection(input), |event| async move {
                let event = mod_test_event(event.clone());
                Ok(Some(test_projection(
                    json!({"text":event["args"].as_str().unwrap()}),
                )))
            })
            .await
            .unwrap();
        let run = mod_test_value(&run);
        assert_eq!(run, json!({"text":"Ada! served"}));
    }

    #[tokio::test]
    async fn sec_default_tool_list_restores_managed_mcp_tools() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("tools.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('tool.list', () => ({ value: [
                { name: 'Read', description: 'Read files', mcp: false },
                { name: 'mcp__user__new', description: 'New user tool', mcp: true },
              ] }));
              on('tool.call', async ($) => ({ result: await $.tool.list() }));
            }
            "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("tools", dir.path(), &module, test_mod_options(json!({})))
            .await
            .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "org-tools".into(),
            turns: 0,
        };
        host.set_sec_default_order(Some(-1));
        let guarded = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let guarded = mod_test_value(&guarded);
        let names: Vec<_> = guarded["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "mcp__org__safe",
                "mcp__audit__log",
                "Read",
                "mcp__user__new"
            ]
        );
        host.set_sec_default_order(None);
        let unguarded = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let unguarded = mod_test_value(&unguarded);
        let names: Vec<_> = unguarded["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Read", "mcp__user__new"]);
    }

    #[tokio::test]
    async fn fs_ancestors_dispatches_operation_and_uses_session() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("ancestors.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('fs.ancestors', async ($, e, next) => {
                if (e.names[0] !== 'AGENTS.md' || next.origin.plugin !== 'ancestors-test') {
                  throw new Error('wrong fs.ancestors input');
                }
                const result = await next(e);
                return { value: result.value.map(entry => ({ ...entry, content: entry.content + '!' })) };
              });
              on('tool.call', async ($) => ({ result: await $.fs.ancestors({ names: ['AGENTS.md'] }) }));
            }
            "#,
        ).unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "ancestors-session".into(),
            turns: 0,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "ancestors-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"][0]["dir"],
            dir.path().to_string_lossy().as_ref()
        );
        assert_eq!(result["result"][0]["name"], "AGENTS.md");
        assert_eq!(result["result"][0]["content"], "memory!");
    }

    #[tokio::test]
    async fn session_surfaces_and_surface_dispatch_through_mod_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("surfaces.js");
        std::fs::write(
            &module,
            r#"
            export function register(on) {
              on('session.surfaces', async ($, e, next) => {
                if (Object.keys(e).length !== 0) throw new Error('wrong surfaces input');
                const answer = await next(e);
                return { value: [...answer.value, 'mobile'] };
              });
              on('session.surface', async ($, e, next) => {
                if (Object.keys(e).length !== 0) throw new Error('wrong surface input');
                const answer = await next(e);
                return { value: answer.value === 'terminal' ? 'desktop' : null };
              });
              on('tool.call', async ($) => ({ result: {
                surfaces: await $.session.surfaces(),
                surface: await $.session.surface(),
              } }));
            }
            "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(dir.path().to_path_buf()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("test".into()),
            id: "surfaces-test".into(),
            turns: 0,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "surfaces-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                &session,
                |_| async { panic!("Mod handles tool.call") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"],
            json!({
                "surfaces": ["terminal", "desktop", "mobile"],
                "surface": "desktop",
            })
        );
    }

    #[tokio::test]
    async fn session_reads_and_file_calls_follow_live_context() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        std::fs::write(one.join("where.txt"), "one").unwrap();
        std::fs::write(two.join("where.txt"), "two").unwrap();
        let module = dir.path().join("session.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('session.model', async ($, e, next) => {
              if (Object.keys(e).length !== 0 || next.origin.plugin !== 'session-test') {
                throw new Error('wrong session.model event');
              }
              const core = await next(e);
              return { value: core.value + '!' };
            });
            on('session.version', async ($, e, next) => {
              if (Object.keys(e).length !== 0) throw new Error('wrong session.version event');
              const core = await next(e);
              return { value: { ...core.value, via: 'hook' } };
            });
            on('tool.call', async ($) => ({ result: {
              cwd: await $.session.cwd(),
              root: await $.session.root(),
              model: await $.session.model(),
              id: await $.session.id(),
              turns: await $.session.turns(),
              version: await $.session.version(),
              file: await $.fs.read('where.txt'),
            } }));
          }
        "#,
        )
        .unwrap();
        let session = TestSessionContext {
            cwd: std::sync::Mutex::new(one.clone()),
            root: dir.path().to_path_buf(),
            model: std::sync::Mutex::new("alpha".into()),
            id: "sess-1".into(),
            turns: 3,
        };
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "session-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let first = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                &session,
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let first = mod_test_value(&first);
        assert_eq!(
            first["result"],
            json!({
                "cwd": one.to_string_lossy(),
                "root": dir.path().to_string_lossy(),
                "model": "alpha!",
            "id": "sess-1",
            "turns": 3,
            "version": {"version":"0.12.0","base":"0.12.0","via":"hook"},
            "file": "one",
            })
        );

        *session.cwd.lock().unwrap() = two.clone();
        *session.model.lock().unwrap() = "beta".into();
        let second = host
            .dispatch_with_log_at_session(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                &session,
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let second = mod_test_value(&second);
        assert_eq!(
            second["result"],
            json!({
                "cwd": two.to_string_lossy(),
                "root": dir.path().to_string_lossy(),
                "model": "beta!",
            "id": "sess-1",
            "turns": 3,
            "version": {"version":"0.12.0","base":"0.12.0","via":"hook"},
            "file": "two",
            })
        );
    }

    #[tokio::test]
    async fn clock_now_ignores_a_hook_result_with_the_wrong_shape() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("bad-clock.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('clock.now', () => ({ value: 'invalid' }));
            on('tool.call', async ($) => {
              const now = await $.clock.now();
              return { result: typeof now === 'number' && now > 0 };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "bad-clock",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { panic!("tool core must be short-circuited") },
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":true}));
    }

    #[tokio::test]
    async fn fs_read_api_uses_session_cwd_and_mod_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.txt"), "from rewritten path").unwrap();
        let module = dir.path().join("read.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('clock.now', () => ({ value: 42 }));
            on('fs.read', { path: 'original.txt' }, async ($, e, next) => {
              const now = await $.clock.now();
              const answer = await next({ ...e, path: now === 42 ? 'other.txt' : 'wrong.txt' });
              return { value: `${answer.value}:wrapped` };
            });
            on('tool.call', async ($) => ({ result: await $.fs.read('original.txt') }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "read-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                dir.path(),
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":"from rewritten path:wrapped"}));
    }

    #[tokio::test]
    async fn fs_read_api_returns_base64_and_rejects_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0_u8, 1, 2]).unwrap();
        std::fs::write(
            dir.path().join("large.bin"),
            vec![0_u8; 4 * 1024 * 1024 + 1],
        )
        .unwrap();
        let module = dir.path().join("bytes.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', async ($) => {
              const { base64 } = await $.fs.read('blob.bin', { as: 'bytes' });
              let oversized = false;
              try { await $.fs.read('large.bin'); }
              catch { oversized = true; }
              return { result: { base64, oversized } };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "bytes-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                dir.path(),
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result, json!({"result":{"base64":"AAEC","oversized":true}}));
    }

    #[tokio::test]
    async fn fs_write_creates_parents_and_respects_rewrite_deny_and_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("write.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('fs.write', { path: 'original.txt' }, async (_, e, next) => {
              const answer = await next({ ...e, path: 'nested/output.txt' });
              if (!Object.hasOwn(answer, 'value') || answer.value !== undefined)
                throw Error('fs.write next did not return { value: undefined }');
              return answer;
            });
            on('fs.write', { path: 'denied.txt' }, () => ({ deny: 'blocked' }));
            on('tool.call', async ($) => {
              const returned = await $.fs.write('original.txt', 'Hello, 世界');
              let denied = false;
              try { await $.fs.write('denied.txt', 'no'); }
              catch (error) { denied = String(error).includes('blocked'); }
              let oversized = false;
              try { await $.fs.write('large.txt', 'x'.repeat(4 * 1024 * 1024 + 1)); }
              catch { oversized = true; }
              return { result: {
                returnedUndefined: returned === undefined,
                denied,
                oversized,
                text: await $.fs.read('nested/output.txt'),
              } };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "write-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Write"})),
                dir.path(),
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result,
            json!({"result":{
                "returnedUndefined":true,
                "denied":true,
                "oversized":true,
                "text":"Hello, 世界"
            }})
        );
        assert!(!dir.path().join("original.txt").exists());
        assert!(!dir.path().join("denied.txt").exists());
        assert!(!dir.path().join("large.txt").exists());
    }

    #[tokio::test]
    async fn fs_exists_list_and_stat_use_host_paths_and_events() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "hello").unwrap();
        let module = dir.path().join("metadata.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('fs.exists', { path: 'virtual.txt' }, () => ({ value: true }));
            on('tool.call', async ($) => {
              const entries = await $.fs.list();
              const stat = await $.fs.stat('file.txt', { resolve: true });
              return { result: {
                file: await $.fs.exists('file.txt'),
                missing: await $.fs.exists('missing.txt'),
                virtual: await $.fs.exists('virtual.txt'),
                names: entries.map(entry => entry.name),
                kind: stat.kind,
                size: stat.size,
                realPath: stat.realPath,
              } };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "metadata-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                dir.path(),
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(result["result"]["file"], true);
        assert_eq!(result["result"]["missing"], false);
        assert_eq!(result["result"]["virtual"], true);
        assert_eq!(result["result"]["kind"], "file");
        assert_eq!(result["result"]["size"], 5);
        assert_eq!(
            result["result"]["realPath"],
            std::fs::canonicalize(dir.path().join("file.txt"))
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(
            result["result"]["names"],
            json!(["file.txt", "metadata.js"])
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fs_metadata_distinguishes_links_from_targets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("target.txt"), "ok").unwrap();
        std::os::unix::fs::symlink("target.txt", dir.path().join("link.txt")).unwrap();
        std::os::unix::fs::symlink("missing.txt", dir.path().join("broken.txt")).unwrap();
        let module = dir.path().join("links.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', async ($) => {
              const list = await $.fs.list();
              const link = await $.fs.stat('link.txt', { resolve: true });
              const broken = await $.fs.stat('broken.txt', { resolve: true });
              return { result: {
                listed: list.filter(e => e.isLink).map(e => [e.name, e.kind]),
                link: { kind: link.kind, isLink: link.isLink, size: link.size,
                        realPath: link.realPath },
                broken: { kind: broken.kind, isLink: broken.isLink,
                          hasRealPath: Object.hasOwn(broken, 'realPath') },
              } };
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "links-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let result = host
            .dispatch_with_log_at(
                "tool.call",
                test_projection(json!({"tool":"Read"})),
                dir.path(),
                |_| async { panic!("tool core must be short-circuited") },
                |_, _| async {},
            )
            .await
            .unwrap();
        let result = mod_test_value(&result);
        assert_eq!(
            result["result"]["listed"],
            json!([["broken.txt", "other"], ["link.txt", "other"]])
        );
        assert_eq!(result["result"]["link"]["kind"], "file");
        assert_eq!(result["result"]["link"]["size"], 2);
        assert_eq!(result["result"]["link"]["isLink"], true);
        assert_eq!(
            result["result"]["link"]["realPath"],
            std::fs::canonicalize(dir.path().join("target.txt"))
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert_eq!(result["result"]["broken"]["kind"], "other");
        assert_eq!(result["result"]["broken"]["isLink"], true);
        assert_eq!(result["result"]["broken"]["hasRealPath"], false);
    }

    #[tokio::test]
    async fn dropping_a_dispatch_aborts_its_signal_and_keeps_worker_usable() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("cancel.js");
        std::fs::write(
            &module,
            r#"
          let aborted = false;
          export function register(on) {
            on('tool.call', ($, e, next) => {
              if (e.tool === 'Check') return { result: aborted };
              $.ui.log('entered');
              return new Promise(resolve => {
                next.signal.addEventListener('abort', () => {
                  aborted = true;
                  resolve({ deny: 'cancelled' });
                }, { once: true });
              });
            });
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "cancel-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let entered_for_call = entered.clone();
        let blocked_host = host.clone();
        let blocked = tokio::spawn(async move {
            blocked_host
                .dispatch_with_log(
                    "tool.call",
                    test_projection(json!({"tool":"Block"})),
                    |_| async { panic!("blocked call should not reach core") },
                    move |_, _| {
                        let entered = entered_for_call.clone();
                        async move { entered.notify_one() }
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("hook must enter");
        blocked.abort();
        let _ = blocked.await;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let result = host
                    .dispatch(
                        "tool.call",
                        test_projection(json!({"tool":"Check"})),
                        |_| async { panic!("check should not reach core") },
                    )
                    .await
                    .unwrap();
                let result = mod_test_value(&result);
                if result == json!({"result":true}) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancel signal must reach the Mod");
    }

    #[tokio::test]
    async fn unload_does_not_interrupt_an_inflight_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("unload.js");
        std::fs::write(
            &module,
            r#"
          export function register(on) {
            on('tool.call', async ($, e, next) => ({ ...await next(e), wrapped: true }));
          }
        "#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load(
            "unload-test",
            dir.path(),
            &module,
            test_mod_options(json!({})),
        )
        .await
        .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let running_host = host.clone();
        let entered_for_core = entered.clone();
        let release_for_core = release.clone();
        let running = tokio::spawn(async move {
            running_host
                .dispatch(
                    "tool.call",
                    test_projection(json!({"tool":"Bash"})),
                    move |_| {
                        let entered = entered_for_core.clone();
                        let release = release_for_core.clone();
                        async move {
                            entered.notify_one();
                            release.notified().await;
                            Ok(Some(test_projection(json!({"result":"core"}))))
                        }
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("core must start");
        tokio::time::timeout(Duration::from_secs(3), host.unload("unload-test"))
            .await
            .expect("unload must not wait for tool core")
            .unwrap();
        release.notify_one();
        let first = running.await.unwrap().unwrap();
        assert!(first.semantically_eq(&test_projection(json!({"result":"core","wrapped":true}))));
        let second = host
            .dispatch(
                "tool.call",
                test_projection(json!({"tool":"Bash"})),
                |_| async { Ok(Some(test_projection(json!({"result":"core"})))) },
            )
            .await
            .unwrap();
        let second = mod_test_value(&second);
        assert_eq!(second, json!({"result":"core"}));
    }
}
