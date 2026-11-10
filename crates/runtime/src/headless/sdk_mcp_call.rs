//! Direct MCP control calls, including bounded file staging. The file lane is
//! a host service; model providers and provider credentials do not select it.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use lingxi_core::host::rooted_fs;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

const FILE_CAP: u64 = 26_214_400;

#[path = "sdk_mcp_staging_schema.rs"]
mod schema;

#[cfg(test)]
#[path = "sdk_mcp_call_tests.rs"]
mod control_tests;

/// A synced file and the source revision actually read by a staged call.
#[derive(Debug)]
pub struct McpStagedFile {
    pub bytes: Vec<u8>,
    pub etag: String,
}

/// Storage failures distinguish conflict/size from retryable transport errors.
#[derive(Debug)]
pub struct McpStagedFileError {
    pub message: String,
    pub retryable: Option<bool>,
    pub conflict: bool,
    pub too_large: bool,
}

impl McpStagedFileError {
    fn failure(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: Some(false),
            conflict: false,
            too_large: false,
        }
    }
}

/// The embedding's synced-row service. Paths are validated relative paths
/// under the virtual `/working` lane, never model-selected filesystem roots.
#[async_trait]
pub trait McpStagedFileStore: Send + Sync {
    async fn read(&self, relative: &str) -> Result<Option<McpStagedFile>, McpStagedFileError>;
    async fn compare_and_write(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        if_match: Option<&str>,
    ) -> Result<String, McpStagedFileError>;
}

/// Concrete local host lane. Every traversal and replacement uses no-follow
/// rooted handles. Per-row leases serialize CAS checks through publication
/// among cooperating calls in this process. Arbitrary external filesystem
/// writers do not participate in those leases; a synced database must supply
/// its own host store for transactional revision checks.
pub struct LocalMcpStagedFileStore {
    root: PathBuf,
}
impl LocalMcpStagedFileStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_local(root: &Path, relative: &str) -> Result<Option<McpStagedFile>, McpStagedFileError> {
    let mut file = match rooted_fs::open_read_file_pinned(root, Path::new(relative), None) {
        Ok(file) => file,
        Err(rooted_fs::FsError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(McpStagedFileError::failure(error.to_string())),
    };
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(FILE_CAP + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| McpStagedFileError::failure(error.to_string()))?;
    if bytes.len() as u64 > FILE_CAP {
        return Err(McpStagedFileError {
            too_large: true,
            ..McpStagedFileError::failure("content too large")
        });
    }
    Ok(Some(McpStagedFile {
        etag: hash(&bytes),
        bytes,
    }))
}

#[async_trait]
impl McpStagedFileStore for LocalMcpStagedFileStore {
    async fn read(&self, relative: &str) -> Result<Option<McpStagedFile>, McpStagedFileError> {
        read_local(&self.root, relative)
    }
    async fn compare_and_write(
        &self,
        relative: &str,
        bytes: Vec<u8>,
        if_match: Option<&str>,
    ) -> Result<String, McpStagedFileError> {
        static LEASES: OnceLock<Mutex<HashMap<(PathBuf, String), Weak<tokio::sync::Mutex<()>>>>> =
            OnceLock::new();
        let lease = {
            let mut leases = LEASES
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            leases.retain(|_, lease| lease.strong_count() != 0);
            let key = (self.root.clone(), relative.to_owned());
            let lease = leases.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
                let lease = Arc::new(tokio::sync::Mutex::new(()));
                leases.insert(key, Arc::downgrade(&lease));
                lease
            });
            lease
        };
        let _lease = lease.lock().await;
        if bytes.len() as u64 > FILE_CAP {
            return Err(McpStagedFileError {
                too_large: true,
                ..McpStagedFileError::failure("content too large")
            });
        }
        if let Some(expected) = if_match {
            if expected.is_empty() {
                return Err(McpStagedFileError::failure("empty if_match"));
            }
            if read_local(&self.root, relative)?.is_none_or(|file| file.etag != expected) {
                return Err(McpStagedFileError {
                    conflict: true,
                    ..McpStagedFileError::failure("lane row changed since if_match")
                });
            }
        }
        let pin = rooted_fs::root_identity(&self.root)
            .map_err(|error| McpStagedFileError::failure(error.to_string()))?;
        let check = || match if_match {
            Some(expected)
                if read_local(&self.root, relative)
                    .map_err(|error| rooted_fs::FsError::Io(error.message))?
                    .is_none_or(|file| file.etag != expected) =>
            {
                Err(rooted_fs::FsError::Io(
                    "lane row changed since if_match".into(),
                ))
            }
            _ => Ok(()),
        };
        rooted_fs::atomic_write_stream_pinned_checked(
            &self.root,
            Path::new(relative),
            rooted_fs::AtomicWriteOptions {
                overwrite: true,
                create_parents: true,
                dir_mode: 0o700,
                file_mode: 0o600,
            },
            &pin,
            |file| {
                file.write_all(&bytes)
                    .map_err(|error| rooted_fs::FsError::Io(error.to_string()))
            },
            check,
        )
        .map_err(|error| McpStagedFileError {
            conflict: error
                .error
                .to_string()
                .contains("lane row changed since if_match"),
            ..McpStagedFileError::failure(error.to_string())
        })?;
        Ok(hash(&bytes))
    }
}

pub(super) struct CallEnvironment {
    pub essential_traffic_only: bool,
    pub program_result: bool,
    pub checked: bool,
    pub file_store: Option<Arc<dyn McpStagedFileStore>>,
}

struct FileSpec {
    name: String,
    lane: String,
    relative: String,
    if_match: Option<String>,
}

fn object(fields: Vec<(&str, Json)>) -> Json {
    Json::object(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
    .expect("generated MCP control fields are unique")
}
fn text(value: impl Into<String>) -> Json {
    Json::from(value.into())
}
fn error(code: &str, detail: impl Into<String>, retryable: Option<bool>) -> Json {
    error_source(code, text(detail), retryable)
}
fn error_source(code: &str, detail: Json, retryable: Option<bool>) -> Json {
    let mut units = detail
        .string_units("")
        .unwrap_or_default()
        .into_iter()
        .take(2000)
        .collect::<Vec<_>>();
    if units
        .last()
        .is_some_and(|last| (0xd800..=0xdbff).contains(last))
    {
        units.pop();
    }
    let detail = Json::parse(&format!(
        "\"{}\"",
        units
            .iter()
            .map(|unit| format!("\\u{unit:04x}"))
            .collect::<String>()
    ))
    .unwrap();
    let mut fields = vec![
        ("ok", Json::parse("false").unwrap()),
        ("error_code", text(code)),
        ("detail", detail),
    ];
    if let Some(retryable) = retryable {
        fields.push((
            "retryable",
            Json::parse(if retryable { "true" } else { "false" }).unwrap(),
        ));
    }
    object(vec![("staging", object(fields))])
}
fn relative_lane(path: &str) -> Result<String, String> {
    if path.contains('\0') {
        return Err("path contains null bytes".into());
    }
    if !path.starts_with('/') {
        return Err("path must be absolute".into());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            value => parts.push(value),
        }
    }
    if parts.first() != Some(&"working") || parts.len() < 2 {
        return Err("path must be under the working sync dir".into());
    }
    let relative = parts[1..].join("/");
    let reserved = relative.replace('\\', "/");
    if reserved == ".ccr-seed" || reserved.starts_with(".ccr-seed/") {
        return Err("/working/.ccr-seed is reserved".into());
    }
    Ok(relative)
}
fn specs(request: &Json, key: &str) -> Result<Option<Vec<FileSpec>>, String> {
    let Some(source) = request.get_property(key) else {
        return Ok(None);
    };
    let count = source
        .array_len()
        .ok_or_else(|| format!("invalid request: {key} must be an array"))?;
    if count > 64 {
        return Err(format!(
            "invalid request: {key} contains more than 64 files"
        ));
    }
    let mut names = HashSet::new();
    let mut paths = HashMap::new();
    let mut files = Vec::new();
    let direction = if key == "input_files" {
        "input"
    } else {
        "output"
    };
    for index in 0..count {
        let file = source
            .get_index(index)
            .ok_or_else(|| "invalid request: file declaration must be an object".to_owned())?;
        let name = file
            .get_property("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "invalid request: file name must be a string".to_owned())?;
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err("invalid request: file name must be 1-64 letters, digits, _ or -".into());
        }
        let lane = file
            .get_property("lane_path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "invalid request: lane_path must be a string".to_owned())?;
        let relative = relative_lane(lane).map_err(|detail| {
            format!(
                "invalid {direction} lane_path {}: {detail}",
                serde_json::to_string(lane).unwrap()
            )
        })?;
        if !names.insert(name.to_owned()) {
            return Err(format!("duplicate {direction} name: {name}"));
        }
        if direction == "output" {
            if let Some(previous) = paths.insert(relative.clone(), lane.to_owned()) {
                return Err(format!(
                    "duplicate output lane_path: {lane} collides with {previous}"
                ));
            }
        }
        let if_match = file
            .get_property("if_match")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        files.push(FileSpec {
            name: name.into(),
            lane: lane.into(),
            relative,
            if_match,
        });
    }
    Ok(Some(files))
}
fn token(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix("{{")
        .and_then(|value| value.strip_suffix("}}"))
    else {
        return false;
    };
    let Some((direction, name)) = inner.split_once(':') else {
        return false;
    };
    matches!(direction, "in" | "out")
        && !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}
fn rewrite(value: &Json, paths: &HashMap<String, String>, seen: &mut Vec<String>) -> Json {
    if let Some(value_string) = value.string_at("") {
        if token(value_string) {
            if !seen.iter().any(|item| item == value_string) {
                seen.push(value_string.into());
            }
            if let Some(path) = paths.get(value_string) {
                return text(path.clone());
            }
        }
        return value.clone();
    }
    if let Some(count) = value.array_len("") {
        return Json::array(
            (0..count)
                .map(|index| {
                    rewrite(
                        &value.subprojection(&format!("/{index}")).unwrap(),
                        paths,
                        seen,
                    )
                })
                .collect(),
        )
        .unwrap();
    }
    if let Some(entries) = value.object_entries() {
        return Json::object_units(
            entries
                .into_iter()
                .map(|entry| {
                    let child = value
                        .subprojection_property_units(entry.code_units)
                        .unwrap();
                    (entry.code_units.to_vec(), rewrite(&child, paths, seen))
                })
                .collect(),
        )
        .unwrap();
    }
    value.clone()
}

struct TemporaryDirectory(PathBuf);
impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn response(result: lingxi_core::host::McpToolResultDto, staging: Option<Json>) -> Json {
    let mut fields = vec![("content", result.content)];
    if let Some(structured) = result.structured_content {
        fields.push(("structuredContent", structured));
    }
    if let Some(meta) = result.meta {
        fields.push(("_meta", meta));
    }
    if let Some(staging) = staging {
        fields.push(("staging", staging));
    }
    object(fields)
}

pub(super) async fn execute(
    orchestrator: &Arc<orchestrator::ConversationOrchestrator>,
    tool: &str,
    request: &Json,
    environment: CallEnvironment,
    cancel: CancellationToken,
) -> Result<Json, Json> {
    let tool_source = request
        .subprojection("/tool")
        .unwrap_or_else(|_| tool.into());
    orchestrator
        .prepare_mcp_control_call_source(&tool_source, cancel.clone())
        .await?;
    let input = request
        .subprojection("/arguments")
        .ok()
        .filter(|value| !value.at_pointer("").is_some_and(|value| value.is_null()))
        .unwrap_or_else(|| Json::parse("{}").unwrap());
    let staged = ["input_files", "output_files", "expires_at", "timeout_ms"]
        .iter()
        .any(|key| request.contains_property(key));
    if !staged {
        return orchestrator
            .call_mcp_control_tool_source(
                &tool_source,
                input,
                environment.checked,
                environment.program_result,
                cancel,
            )
            .await
            .map(|result| response(result, None));
    }
    if let Some(refusal) = schema::refusal(request) {
        return Ok(refusal);
    }
    let staged_result = async {
    if cfg!(windows) {
        return Ok(error(
            "tool_error",
            "staged mcp_call is not supported on this platform",
            None,
        ));
    }
    if request.contains_property("output_files") && !request.contains_property("expires_at") {
        return Ok(error(
            "tool_error",
            "invalid request: expires_at is required when output_files are declared",
            None,
        ));
    }
    if let Some(expires) = request.get_property("expires_at") {
        let Some(expires) = expires.as_str() else {
            return Ok(error(
                "tool_error",
                "invalid request: expires_at must be a string",
                None,
            ));
        };
        let deadline = chrono::DateTime::parse_from_rfc3339(expires);
        let Ok(deadline) = deadline else {
            return Ok(error(
                "expired",
                format!("expires_at is not an RFC3339 timestamp with a zone: {expires}"),
                None,
            ));
        };
        if chrono::Utc::now() > deadline {
            return Ok(error(
                "expired",
                format!("request expired at {expires}"),
                None,
            ));
        }
    }
    if input.object_entries().is_none() {
        return Ok(error(
            "tool_error",
            "invalid request: arguments must be an object",
            None,
        ));
    }
    let inputs = match specs(request, "input_files") {
        Ok(value) => value,
        Err(detail) => return Ok(error("tool_error", detail, None)),
    };
    let outputs = match specs(request, "output_files") {
        Ok(value) => value,
        Err(detail) => return Ok(error("tool_error", detail, None)),
    };
    let files_declared = inputs.is_some() || outputs.is_some();
    let inputs = inputs.unwrap_or_default();
    let outputs = outputs.unwrap_or_default();
    let mut seen = Vec::new();
    rewrite(&input, &HashMap::new(), &mut seen);
    if files_declared {
        let declared = inputs
            .iter()
            .map(|file| format!("{{{{in:{}}}}}", file.name))
            .chain(
                outputs
                    .iter()
                    .map(|file| format!("{{{{out:{}}}}}", file.name)),
            )
            .collect::<HashSet<_>>();
        let unknown = seen
            .iter()
            .filter(|name| !declared.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        if !unknown.is_empty() {
            return Ok(error(
                "tool_error",
                format!(
                    "invalid request: args token names no declared file: {}",
                    unknown.join(", ")
                ),
                None,
            ));
        }
        let unused = outputs
            .iter()
            .filter(|file| !seen.contains(&format!("{{{{out:{}}}}}", file.name)))
            .map(|file| file.name.clone())
            .collect::<Vec<_>>();
        if !unused.is_empty() {
            return Ok(error("tool_error",format!("invalid request: declared output not referenced by any {{{{out:NAME}}}} token in arguments: {}",unused.join(", ")),None));
        }
    }
    let directory =
        std::env::temp_dir().join(format!("mcp-tool-{}", uuid::Uuid::new_v4().simple()));
    if let Err(error) = std::fs::create_dir(&directory) {
        return Ok(self::error(
            "tool_error",
            format!(
                "staging unavailable: {}",
                error
                    .raw_os_error()
                    .map_or_else(|| "temp_dir_refused".into(), |code| code.to_string())
            ),
            None,
        ));
    }
    let _cleanup = TemporaryDirectory(directory.clone());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    for subdirectory in ["in", "out"] {
        std::fs::create_dir(directory.join(subdirectory)).map_err(|error| error.to_string())?;
    }
    let store = environment.file_store;
    if let Some(file) = inputs.first().filter(|_| environment.essential_traffic_only) {
        return Ok(error("tool_error", format!("input fetch failed for {}: get gated: essential-traffic-only", file.lane), Some(false)));
    }
    if (!inputs.is_empty() || !outputs.is_empty()) && store.is_none() {
        return Ok(error("tool_error", "staging unavailable: host file lane is not configured", Some(false)));
    }
    let mut paths = HashMap::new();
    let mut used = Vec::new();
    for (index, file) in inputs.iter().enumerate() {
        if cancel.is_cancelled() {
            return Ok(error("tool_error", "cancelled by client", None));
        }
        if environment.essential_traffic_only {
            return Ok(error(
                "tool_error",
                format!(
                    "input fetch failed for {}: get gated: essential-traffic-only",
                    file.lane
                ),
                Some(false),
            ));
        }
        let source = match store.as_ref().expect("file lane validated").read(&file.relative).await {
            Ok(Some(source)) => source,
            Ok(None) => {
                return Ok(error(
                    "input_missing",
                    format!("input not found: {}", file.lane),
                    None,
                ))
            }
            Err(failure) => {
                return Ok(error(
                    "tool_error",
                    format!("input fetch failed for {}: {}", file.lane, failure.message),
                    failure.retryable,
                ))
            }
        };
        let path = directory
            .join("in")
            .join(format!("{index}{}", extension(&file.lane)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut target = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|error| error.to_string())?;
            target
                .write_all(&source.bytes)
                .map_err(|error| error.to_string())?;
        }
        paths.insert(
            format!("{{{{in:{}}}}}", file.name),
            path.to_string_lossy().into_owned(),
        );
        used.push(object(vec![
            ("lane_path", text(file.lane.clone())),
            ("etag", text(source.etag)),
        ]));
    }
    let destinations = outputs
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let path = directory
                .join("out")
                .join(format!("{index}{}", extension(&file.lane)));
            paths.insert(
                format!("{{{{out:{}}}}}", file.name),
                path.to_string_lossy().into_owned(),
            );
            path
        })
        .collect::<Vec<_>>();
    let input = if files_declared {
        rewrite(&input, &paths, &mut Vec::new())
    } else {
        input
    };
    let timeout = request.number_at("/timeout_ms").unwrap_or(120_000.0);
    if !timeout.is_finite() {
        return Ok(error(
            "tool_error",
            "invalid request: timeout_ms must be a number",
            None,
        ));
    }
    let timeout = timeout.clamp(1000.0, 600_000.0) as u64;
    let timed = cancel.child_token();
    let cancellation_reason = Arc::new(std::sync::Mutex::new("AbortError: remote-cancel".into()));
    let work = orchestrator.call_mcp_control_tool_source_with_reason(
        &tool_source,
        input,
        environment.checked,
        environment.program_result,
        timed.clone(),
        cancellation_reason.clone(),
    );
    tokio::pin!(work);
    let completed = tokio::select! {
        result = &mut work => result,
        _ = tokio::time::sleep(Duration::from_millis(timeout)) => {
            *cancellation_reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = "Error: staged mcp_call timed out".into();
            timed.cancel();
            return Ok(error(
                "timeout",
                format!("tool call exceeded {timeout}ms"),
                None,
            ));
        }
    };
    let result = match completed {
        Err(detail) => {
            let mut units = if cancel.is_cancelled() { "cancelled by client (" } else { "tool call failed: " }.encode_utf16().collect::<Vec<_>>();
            units.extend(detail.string_units("").unwrap_or_default());
            if cancel.is_cancelled() { units.push(u16::from(b')')); }
            return Ok(error_source("tool_error", Json::root_string(String::from_utf16_lossy(&units), units).unwrap(), None))
        }
        Ok(result) => result,
    };
    let root = std::fs::canonicalize(&directory).map_err(|error| error.to_string())?;
    let mut written = Vec::new();
    for (file, path) in outputs.iter().zip(destinations) {
        if cancel.is_cancelled() {
            return Ok(error("tool_error", "cancelled by client", None));
        }
        let canonical = match std::fs::canonicalize(&path) {
            Ok(path) => path,
            Err(_) => {
                return Ok(error(
                    "tool_error",
                    format!("tool did not produce output: {}", file.name),
                    None,
                ))
            }
        };
        if !canonical.starts_with(&root) {
            return Ok(error(
                "tool_error",
                format!("output path escapes the staging dir: {}", file.name),
                None,
            ));
        }
        let mut handle =
            rooted_fs::open_read_file_pinned(&root, canonical.strip_prefix(&root).unwrap(), None)
                .map_err(|error| error.to_string())?;
        let metadata = handle.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Ok(error(
                "tool_error",
                format!("output is not a regular file: {}", file.name),
                None,
            ));
        }
        if output_link_count(&handle).map_err(|error| error.to_string())? > 1 {
            return Ok(error(
                "tool_error",
                format!("output has multiple hard links: {}", file.name),
                None,
            ));
        }
        if metadata.len() > FILE_CAP {
            return Ok(error(
                "output_too_large",
                format!(
                    "output {} is {} bytes (cap {FILE_CAP})",
                    file.name,
                    metadata.len()
                ),
                None,
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut handle)
            .take(metadata.len() + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 != metadata.len() {
            return Ok(error(
                "tool_error",
                format!("output changed during collection: {}", file.name),
                None,
            ));
        }
        if environment.essential_traffic_only {
            return Ok(error(
                "tool_error",
                format!(
                    "output write failed for {}: put gated: essential-traffic-only",
                    file.lane
                ),
                Some(false),
            ));
        }
        let size = bytes.len();
        match store.as_ref().expect("file lane validated")
            .compare_and_write(&file.relative, bytes, file.if_match.as_deref())
            .await
        {
            Ok(etag) => written.push(object(vec![
                ("lane_path", text(file.lane.clone())),
                ("etag", text(etag)),
                ("bytes", Json::parse(&size.to_string()).unwrap()),
            ])),
            Err(failure) => {
                return Ok(error(
                    if failure.conflict {
                        "output_conflict"
                    } else if failure.too_large {
                        "output_too_large"
                    } else {
                        "tool_error"
                    },
                    format!("output write failed for {}: {}", file.lane, failure.message),
                    failure.retryable,
                ))
            }
        }
    }
    Ok(response(
        result,
        Some(object(vec![
            ("ok", Json::parse("true").unwrap()),
            ("outputs", Json::array(written).unwrap()),
            ("inputs_used", Json::array(used).unwrap()),
        ])),
    ))
    }.await;
    Ok(staged_result.unwrap_or_else(|detail: String| {
        error("tool_error", format!("unexpected failure: {detail}"), None)
    }))
}

fn extension(path: &str) -> String {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !extension.is_empty()
        && extension.len() <= 8
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        format!(".{extension}")
    } else {
        String::new()
    }
}

fn output_link_count(file: &std::fs::File) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(file.metadata()?.nlink())
    }
    #[cfg(windows)]
    {
        rooted_fs::file_link_count(file).map(u64::from)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "file link metadata is unavailable",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_staged_lane_cas_is_atomic_for_cooperating_host_calls() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalMcpStagedFileStore::new(root.path().to_path_buf()));
        let original = store
            .compare_and_write("sub/result.txt", b"one".to_vec(), None)
            .await
            .unwrap();
        let read = store.read("sub/result.txt").await.unwrap().unwrap();
        assert_eq!(read.bytes, b"one");
        assert_eq!(read.etag, original);
        let (a, b) = tokio::join!(
            store.compare_and_write("sub/result.txt", b"two".to_vec(), Some(&original)),
            store.compare_and_write("sub/result.txt", b"three".to_vec(), Some(&original)),
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert!(a.as_ref().err().or(b.as_ref().err()).unwrap().conflict);
        let current = store.read("sub/result.txt").await.unwrap().unwrap();
        assert!(matches!(current.bytes.as_slice(), b"two" | b"three"));
        assert!(
            store
                .compare_and_write("sub/result.txt", b"stale".to_vec(), Some(&original))
                .await
                .unwrap_err()
                .conflict
        );
        assert!(store
            .compare_and_write("sub/result.txt", b"empty".to_vec(), Some(""))
            .await
            .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_staged_lane_rejects_symlink_and_parent_escapes() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
        let store = LocalMcpStagedFileStore::new(root.path().to_path_buf());
        assert!(store.read("linked/secret").await.is_err());
        assert!(store
            .compare_and_write("linked/secret", b"bad".to_vec(), None)
            .await
            .is_err());
        assert!(store
            .compare_and_write("../escape", b"bad".to_vec(), None)
            .await
            .is_err());
        assert_eq!(
            std::fs::read(outside.path().join("secret")).unwrap(),
            b"outside"
        );
    }

    #[test]
    fn staged_tokens_replace_whole_values_and_preserve_exact_keys() {
        let input = Json::parse(r#"{"\ud800":"{{in:a}}","array":["{{out:b}}","prefix {{in:a}}"]}"#)
            .unwrap();
        let paths = HashMap::from([
            ("{{in:a}}".into(), "/chosen/in".into()),
            ("{{out:b}}".into(), "/chosen/out".into()),
        ]);
        let rewritten = rewrite(&input, &paths, &mut Vec::new());
        assert_eq!(
            rewritten
                .subprojection_property_units(&[0xd800])
                .unwrap()
                .string_at(""),
            Some("/chosen/in")
        );
        assert_eq!(rewritten.string_at("/array/0"), Some("/chosen/out"));
        assert_eq!(rewritten.string_at("/array/1"), Some("prefix {{in:a}}"));
        for value in [
            "relative",
            "/working/..",
            "/other/a",
            "/working/.ccr-seed/a",
            "/working/a\0b",
        ] {
            assert!(relative_lane(value).is_err(), "{value:?}");
        }
        assert_eq!(relative_lane("/working/a/../b").unwrap(), "b");
    }
}
