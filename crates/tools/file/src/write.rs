//! `FileWriteTool` — write a UTF-8 file inside the trusted-dirs whitelist.
//!
//! Behaviour (1:1 with claude-code `FileWriteTool.ts`):
//! - The schema is `{file_path, content}` only — there is NO `mkdir` flag.
//! - Parent directories are created UNCONDITIONALLY before the write
//!   (`FileWriteTool.ts:254` `mkdir(dir, recursive)`).
//! - Overwrite is allowed unconditionally.
//! - New files use UTF-8. Existing UTF-16LE encoding and leading BOMs persist.

use crate::Utf16JsonProjection;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use telemetry::otel;
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{WRITE_COMPLETED, WRITE_FAILED, WRITE_STARTED};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, translate_model_path,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock — matches claude-code tool registry.
pub const TOOL_NAME: &str = "Write";

/// The LONG Write prompt — byte-locked VERBATIM to claude-code `v0i(e)`'s
/// `Dh(e)===false` branch (binary offset ~196432410): the base text plus
/// `${ZAd()}` (the read-first line, `${Ws}`=Read) appended to the overwrite
/// bullet, spelled out inline here. `${Ua}`=Edit. Em-dash is U+2014.
const WRITE_PROMPT_LONG: &str = include_str!("templates/claude-code-2.1.319/write-long.md");

/// The SHORT Write prompt — byte-locked VERBATIM to claude-code `v0i(e)`'s
/// `Dh(e)===true` branch (binary offset 196432581), served to current-gen
/// default models. `${Ws}`=Read, `${Ua}`=Edit. Em-dash is U+2014.
const WRITE_PROMPT_SHORT: &str = include_str!("templates/claude-code-2.1.319/write-lean.md");

/// Build the model-facing `tool_result` message for a Write, byte-faithful to
/// claude-code `FileWriteTool.mapToolResultToToolResultBlockParam`
/// (`FileWriteTool.ts:418-433`): `create` → `"File created successfully at:
/// {path}"`, `update` → `"The file {path} has been updated successfully."`.
///
/// `path` is the ORIGINAL `file_path` input string (claude-code echoes the
/// caller's path verbatim, not a canonicalized form).
#[must_use]
pub fn write_result_message(path: &str, is_create: bool) -> String {
    let base = if is_create {
        format!("File created successfully at: {path}")
    } else {
        format!("The file {path} has been updated successfully.")
    };
    format!("{base}{}", crate::FILE_STATE_CURRENT_SUFFIX)
}

/// `FileWriteTool` — writes a UTF-8 file inside the trusted-dirs whitelist.
pub struct FileWriteTool {
    ctx: BuiltinToolContext,
}

fn open_write_rooted_file(
    requested: &std::path::Path,
    approved: &std::path::Path,
    trusted_dirs: &[std::path::PathBuf],
) -> Result<std::fs::File, lingxi_core::host::rooted_fs::RootedFsError> {
    let Some((root, relative)) = crate::shared::rooted_location(approved, trusted_dirs) else {
        return Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
            lingxi_core::host::FsError::OutsideWorkspace(approved.display().to_string()),
        ));
    };
    lingxi_core::host::rooted_fs::open_file_after_permission(&root, &relative, requested, approved)
}

/// Read only the length admitted from the opened descriptor. A growing file
/// cannot keep a preparation worker allocating indefinitely; the checked write
/// still compares the complete snapshot before changing the file.
fn read_write_snapshot(
    mut file: std::fs::File,
    metadata: std::fs::Metadata,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<
    lingxi_core::host::rooted_fs::RootedFileSnapshot,
    lingxi_core::host::rooted_fs::RootedFsError,
> {
    use lingxi_core::host::rooted_fs::{RootedFileSnapshot, RootedFsError};

    let size = metadata.len();
    let bytes = read_write_snapshot_bytes(&mut file, size, cancel)?;
    let metadata = file
        .metadata()
        .map_err(|error| RootedFsError::Fs(lingxi_core::host::FsError::Io(error.to_string())))?;
    if metadata.len() != size {
        return Err(RootedFsError::ContentChanged);
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.mode())
    };
    #[cfg(not(unix))]
    let mode = None;
    Ok(RootedFileSnapshot {
        bytes,
        size,
        modified: metadata.modified().ok(),
        mode,
    })
}

fn read_write_snapshot_bytes(
    file: &mut impl std::io::Read,
    size: u64,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Vec<u8>, lingxi_core::host::rooted_fs::RootedFsError> {
    use lingxi_core::host::rooted_fs::RootedFsError;

    let mut bytes = Vec::new();
    let mut chunk = [0; 64 * 1024];
    loop {
        if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return Err(RootedFsError::Cancelled);
        }
        let remaining = size
            .saturating_add(1)
            .saturating_sub(bytes.len() as u64)
            .min(chunk.len() as u64) as usize;
        if remaining == 0 {
            return Err(RootedFsError::ContentChanged);
        }
        let length = match file.read(&mut chunk[..remaining]) {
            Ok(length) => length,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(RootedFsError::Fs(lingxi_core::host::FsError::Io(
                    error.to_string(),
                )));
            }
        };
        if length == 0 {
            break;
        }
        bytes.try_reserve(length).map_err(|error| {
            RootedFsError::Fs(lingxi_core::host::FsError::Io(error.to_string()))
        })?;
        bytes.extend_from_slice(&chunk[..length]);
        if bytes.len() as u64 > size {
            return Err(RootedFsError::ContentChanged);
        }
    }
    if bytes.len() as u64 != size {
        return Err(RootedFsError::ContentChanged);
    }
    Ok(bytes)
}

#[derive(Debug)]
enum WriteSnapshotError {
    StartOnly(tool_api::read_file_state::StartOnlyWriteBlock),
    PartialRead,
    MissingRead,
    Read(lingxi_core::host::rooted_fs::RootedFsError),
}

/// Read admission depends only on metadata and model context. Keep it in front
/// of the only existing-file body reader, including for private memory forks.
fn admitted_write_snapshot(
    file: Option<(std::fs::File, std::fs::Metadata)>,
    state: &tool_api::read_file_state::ReadFileStateWriteContext,
    requested: &Path,
    approved: &Path,
    waived: bool,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Option<lingxi_core::host::rooted_fs::RootedFileSnapshot>, WriteSnapshotError> {
    if let Some(blocked) = state.start_only_write_guard(requested, approved) {
        return Err(WriteSnapshotError::StartOnly(blocked));
    }
    let Some((file, metadata)) = file else {
        return Ok(None);
    };
    let entry = tool_api::read_file_state::get_metadata(&state.map(), requested);
    if entry.is_some_and(|entry| entry.is_partial_view) {
        return Err(WriteSnapshotError::PartialRead);
    }
    if entry.is_none() && !waived {
        return Err(WriteSnapshotError::MissingRead);
    }
    read_write_snapshot(file, metadata, cancel)
        .map(Some)
        .map_err(WriteSnapshotError::Read)
}

/// Resolve a write target without materializing any missing parent through its
/// pathname. `canonicalize_and_validate` can only canonicalize an existing
/// target or its immediate existing parent, so preserve the old create-parents
/// behavior by canonicalizing the nearest existing ancestor and appending the
/// missing lexical components. The rooted write then creates those components
/// through its no-follow directory-handle chain.
fn canonicalize_write_target(path: &Path, trusted_dirs: &[PathBuf]) -> Result<PathBuf, ()> {
    if let Ok(canonical) = canonicalize_and_validate(path, trusted_dirs) {
        return Ok(canonical);
    }

    let mut missing = Vec::new();
    let mut probe = path.to_path_buf();
    while std::fs::symlink_metadata(&probe).is_err() {
        let name = probe.file_name().ok_or(())?.to_os_string();
        missing.push(name);
        probe = probe.parent().ok_or(())?.to_path_buf();
    }
    let mut canonical = canonicalize_and_validate(&probe, trusted_dirs).map_err(|_| ())?;
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

fn write_resolution_error(
    path: &str,
    error: lingxi_core::host::rooted_fs::RootedFsError,
) -> ToolError {
    match error {
        lingxi_core::host::rooted_fs::RootedFsError::ContentChanged => ToolError::InvalidInput(crate::FILE_CONTENT_CHANGED_LINTER_MESSAGE.into()),
        lingxi_core::host::rooted_fs::RootedFsError::Cancelled => ToolError::Aborted,
        lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink => ToolError::InvalidInput(format!(
            "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
        ).into()),
        lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged => {
            ToolError::InvalidInput(format!(
                "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
            ).into())
        }
        lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged => {
            if std::fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
                ).into())
            } else {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
                ).into())
            }
        }
        lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile => {
            ToolError::Io(format!("File {path} is not a regular file").into())
        }
        lingxi_core::host::rooted_fs::RootedFsError::Fs(error) => ToolError::Io(error.to_string().into()),
    }
}

impl FileWriteTool {
    /// Construct a new tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn emit_started(&self, invocation_id: &str, path: &std::path::Path) {
        self.ctx
            .bus
            .log_event_lazy(WRITE_STARTED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "_PROTO_file_path".to_string(),
                    AnalyticsValue::String(
                        PiiTagged::assert_pii_tagged_column(path.display().to_string())
                            .into_inner(),
                    ),
                );
                md
            })
            .await;
    }

    async fn emit_completed(&self, invocation_id: &str, bytes_written: u64, duration_ms: u64) {
        self.ctx
            .bus
            .log_event_lazy(WRITE_COMPLETED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "bytes_written".to_string(),
                    AnalyticsValue::Int(bytes_written as i64),
                );
                md.insert(
                    "duration_ms".to_string(),
                    AnalyticsValue::Int(duration_ms as i64),
                );
                md
            })
            .await;
    }

    async fn emit_failed(&self, invocation_id: &str, kind: &str) {
        self.ctx
            .bus
            .log_event_lazy(WRITE_FAILED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "failure_kind".to_string(),
                    AnalyticsValue::String(Verified::assert_safe(kind.to_string()).into_inner()),
                );
                md
            })
            .await;
    }

    /// Emit `tengu_subagent_md_report_blocked` with the attempted content's
    /// UTF-8 byte length (claude-code `Buffer.byteLength(t)`).
    async fn emit_subagent_md_report_blocked(&self, content: &str) {
        self.ctx
            .bus
            .log_event_lazy(telemetry::tengu::tool::SUBAGENT_MD_REPORT_BLOCKED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "contentBytes".to_string(),
                    AnalyticsValue::Int(i64::try_from(content.len()).unwrap_or(i64::MAX)),
                );
                md
            })
            .await;
    }
}

/// claude-code `/^(REPORT|SUMMARY|FINDINGS|ANALYSIS).*\.md$/i.test(basename(n))`:
/// a basename that (case-insensitively) starts with one of the four report words
/// and ends with `.md`.
fn is_subagent_report_md(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md")
        && ["report", "summary", "findings", "analysis"]
            .iter()
            .any(|w| lower.starts_with(w))
}

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::parse(include_str!(
        "templates/claude-code-2.1.322/write-input-schema.json"
    ))
    .expect("pinned write input schema is valid JSON")
});

static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::parse(include_str!(
        "templates/claude-code-2.1.322/write-output-schema.json"
    ))
    .expect("pinned Write output schema is valid JSON")
});

#[async_trait]
impl Tool for FileWriteTool {
    fn supports_persistent_chat(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("create or overwrite files".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }
    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&OUTPUT_SCHEMA)
    }
    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        crate::output_contract::parse(TOOL_NAME, output)
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        crate::input_repair_metadata()
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_destructive(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        // claude-code `FileWriteTool.ts:100`.
        "Write a file to the local filesystem.".to_string()
    }

    async fn prompt(&self, opts: &PromptOptions) -> Utf16JsonProjection {
        // Model-gated, mirroring claude-code `v0i(e){if(Dh(e))return SHORT;
        // return LONG}` (binary offset ~196432410). Predicate shared with
        // TodoWrite via `tool_api`.
        if tool_api::prompt_uses_lean(opts) {
            WRITE_PROMPT_SHORT.into()
        } else {
            WRITE_PROMPT_LONG.into()
        }
    }

    fn get_path(&self, input: &Utf16JsonProjection) -> Option<PathBuf> {
        crate::input_string(input, "file_path").map(PathBuf::from)
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        crate::mutation_io::check_cancel(ctx.cancel.as_ref())?;
        let invocation_id = tool_api::util::ids::ulid_or_uuid();
        let file_path = crate::input_string(&input, "file_path")
            .ok_or_else(|| ToolError::InvalidInput("file_path is required".into()))?;
        let requested_content = input
            .string_units("/content")
            .ok_or_else(|| ToolError::InvalidInput("content is required".into()))?;
        let content_display = String::from_utf16_lossy(&requested_content);
        let content = content_display.as_str();
        // Worktree parity plan (Task 3): a RELATIVE `file_path` resolves
        // against the CURRENT session cwd (`ctx.cwd()`, switchable by
        // `EnterWorktree`/`ExitWorktree`), not the frozen OS process cwd that
        // `std::fs::canonicalize` would otherwise consult below. An absolute
        // `file_path` (the documented/expected case) is unaffected.
        let requested_path = crate::checked_model_file_path(file_path, &self.ctx.cwd())?;
        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, requested_path.clone(), true) {
            Ok(path) => path,
            Err(message) => return Err(ToolError::InvalidInput(message.into())),
        };

        // A subagent must NOT write a REPORT/SUMMARY/FINDINGS/ANALYSIS `*.md`
        // report file — it should return findings as text (claude-code:
        // `if(r.agentId && /^(REPORT|SUMMARY|FINDINGS|ANALYSIS).*\.md$/i
        // .test(basename(n))) return {result:!1, message:…, errorCode:5}` plus
        // `tengu_subagent_md_report_blocked`). `ctx.agent_id.is_some()` is the
        // `r.agentId` analog (populated only inside a subagent; `None` on the main
        // thread / leader), so a main-agent write is unaffected. Runs before the
        // started/permission path, mirroring CC's validateInput placement.
        if ctx.agent_id.is_some() && is_subagent_report_md(&path) {
            self.emit_subagent_md_report_blocked(content).await;
            return Err(ToolError::InvalidInput(
                "Subagents should return findings as text, not write report files. Include this content in your final response instead.".into(),
            ));
        }

        let memory_mutation =
            lingxi_core::host::memory_tool_scope::memory_file_mutation(TOOL_NAME, &input)
                .map_err(|error| ToolError::InvalidInput(error.to_string().into()))?;
        let started = Instant::now();
        self.emit_started(&invocation_id, &path).await;

        // One snapshot of trusted_dirs for both containment checks below (the
        // parent-ancestor probe and the final target check), so they can't
        // straddle a swap between the two calls.
        let mut trusted_dirs = memory_mutation.as_ref().map_or_else(
            || self.ctx.trusted_dirs_for_tool(TOOL_NAME, &input),
            |scope| vec![scope.directory().to_owned()],
        );
        if let Some(root) =
            lingxi_core::host::teammate_plan::own_plan_file_root(ctx.agent_id.as_ref(), &path)
        {
            trusted_dirs.push(root);
        }

        // Parents are created unconditionally, matching claude-code, but the
        // creation is performed only after permission/containment checks by the
        // rooted no-follow mkdirat/handle chain. Never call create_dir_all on
        // the model pathname: a retargeted ancestor could create outside the
        // approved tree before the rooted write gets a chance to reject it.
        let canon = if let Some(scope) = &memory_mutation {
            scope.target()
        } else {
            let requested = path.clone();
            let roots = trusted_dirs.clone();
            match crate::mutation_io::prepare(ctx.cancel.as_ref(), move || {
                canonicalize_write_target(&requested, &roots)
            })
            .await?
            {
                Ok(p) => p,
                Err(_) => {
                    emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                    self.emit_failed(&invocation_id, "path_blocked").await;
                    return Err(ToolError::PathBlocked { path });
                }
            }
        };

        // Admit an opened regular file before reading its body. Existing empty
        // files still require a Read, although native Write reports their
        // successful replacement as `create` (`if (oldContent)`). Keep this
        // descriptor for the snapshot so admission and its length bound refer
        // to the same file object.
        let snapshot_path = path.clone();
        let snapshot_canon = canon.clone();
        let snapshot_roots = trusted_dirs.clone();
        let snapshot_scope = memory_mutation.clone();
        let opened = crate::mutation_io::prepare(ctx.cancel.as_ref(), move || {
            let file = if let Some(scope) = &snapshot_scope {
                scope.open_file()
            } else {
                open_write_rooted_file(&snapshot_path, &snapshot_canon, &snapshot_roots)
            }?;
            let metadata = file.metadata().map_err(|error| {
                lingxi_core::host::rooted_fs::RootedFsError::Fs(lingxi_core::host::FsError::Io(
                    error.to_string(),
                ))
            })?;
            Ok((file, metadata))
        })
        .await?;
        let prior_file = match opened {
            Ok(file) => Some(file),
            Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
                lingxi_core::host::FsError::NotFound(_),
            )) => None,
            Err(error @ lingxi_core::host::rooted_fs::RootedFsError::ContentChanged)
            | Err(error @ lingxi_core::host::rooted_fs::RootedFsError::Cancelled)
            | Err(error @ lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink)
            | Err(
                error @ lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged,
            )
            | Err(error @ lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged) => {
                self.emit_failed(&invocation_id, "symlink_resolution_changed")
                    .await;
                return Err(write_resolution_error(file_path, error));
            }
            Err(lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile) => {
                self.emit_failed(&invocation_id, "not_regular_file").await;
                let message = if std::fs::metadata(&canon).is_ok_and(|meta| meta.is_dir()) {
                    format!("{file_path} is a directory, not a file. To create a file inside it, include the file name in file_path.")
                } else {
                    format!("{file_path} exists but is not a regular file (a device, FIFO or socket). Write only creates or overwrites regular files.")
                };
                return Err(ToolError::InvalidInput(message.into()));
            }
            // A non-ENOENT read error means the path exists but is unreadable;
            // treat as "no prior content" and let the write operation report
            // its own error, matching the pre-hardening behavior.
            Err(_) => None,
        };

        let snapshot_state =
            tool_api::read_file_state::capture_read_file_state_write(&self.ctx.read_file_state);
        let snapshot_requested = requested_path.clone();
        let snapshot_canon = canon.clone();
        let snapshot_cancel = ctx.cancel.clone();
        let waived = crate::read_requirement_waived(Some(&ctx.options.main_loop_model), &canon);
        let prior_snapshot = match crate::mutation_io::prepare(ctx.cancel.as_ref(), move || {
            admitted_write_snapshot(
                prior_file,
                &snapshot_state,
                &snapshot_requested,
                &snapshot_canon,
                waived,
                snapshot_cancel.as_ref(),
            )
        })
        .await?
        {
            Ok(snapshot) => snapshot,
            Err(WriteSnapshotError::StartOnly(blocked)) => {
                return Err(ToolError::InvalidInput(blocked.message().into()));
            }
            Err(WriteSnapshotError::PartialRead) => {
                self.emit_failed(&invocation_id, "partial_read").await;
                return Err(ToolError::InvalidInput(crate::FILE_NOT_READ_ERROR.into()));
            }
            Err(WriteSnapshotError::MissingRead) => {
                self.emit_failed(&invocation_id, "stale_read").await;
                return Err(ToolError::InvalidInput(crate::FILE_NOT_READ_ERROR.into()));
            }
            Err(WriteSnapshotError::Read(error)) => {
                self.emit_failed(&invocation_id, "io_read").await;
                return Err(write_resolution_error(file_path, error));
            }
        };
        let prior_mtime_ms = prior_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.modified)
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        let (is_create, file_exists, prior_decoded, encoding, not_text, write_expectation) =
            crate::mutation_io::prepare(ctx.cancel.as_ref(), move || match prior_snapshot {
                Some(snapshot) => {
                    let expectation =
                        lingxi_core::host::rooted_fs::RootedWriteExpectation::from_bytes(
                            &snapshot.bytes,
                        );
                    let not_text = crate::file_meta::classify_not_text(&snapshot.bytes);
                    let (decoded, encoding, _) =
                        crate::file_meta::read_with_metadata_units(&snapshot.bytes);
                    (
                        decoded.is_empty(),
                        true,
                        Some(decoded),
                        encoding,
                        not_text,
                        expectation,
                    )
                }
                None => (
                    true,
                    false,
                    None,
                    crate::file_meta::Encoding::Utf8,
                    None,
                    lingxi_core::host::rooted_fs::RootedWriteExpectation::Missing,
                ),
            })
            .await?;

        // Read-before-write staleness guard (Batch F): only for an EXISTING
        // file. New-file creation skips it (TS `meta === null` skips the
        // guard; `FileWriteTool.ts:198-219` & `:279-295`).
        if file_exists {
            // Native Write refuses a truncated context even when its mtime is
            // unchanged. Model waivers apply only when no entry exists.
            if tool_api::read_file_state::get_metadata(&self.ctx.read_file_state, &requested_path)
                .is_some_and(|entry| entry.is_partial_view)
            {
                self.emit_failed(&invocation_id, "partial_read").await;
                return Err(ToolError::InvalidInput(crate::FILE_NOT_READ_ERROR.into()));
            }
            // Current mtime (floored ms), captured from the same opened handle
            // as the pre-write bytes; `None` falls back to epoch `0`.
            let current_mtime_ms = prior_mtime_ms;
            let cmp_content =
                crate::utf16_mutation::without_bom(prior_decoded.as_deref().unwrap_or(&[]));
            if let Err(e) = crate::check_read_before_write_units(
                &self.ctx.read_file_state,
                &requested_path,
                current_mtime_ms,
                cmp_content,
                crate::read_requirement_waived(Some(&ctx.options.main_loop_model), &canon),
            ) {
                self.emit_failed(&invocation_id, "stale_read").await;
                return Err(e);
            }
        }

        if let Some(kind) = not_text {
            self.emit_failed(&invocation_id, "unsupported_encoding")
                .await;
            return Err(ToolError::InvalidInput(
                kind.mutation_refusal_message().into(),
            ));
        }
        // Exact line diffing can be quadratic for a small shuffled file. Run
        // it on a bounded preparation worker, before the backup/commit, so
        // cancellation or a failed preparation cannot follow a disk mutation.
        let (
            prior_decoded,
            content_units,
            bytes,
            published_units,
            structured_patch,
            lines_added,
            lines_removed,
        ) = crate::mutation_io::prepare(ctx.cancel.as_ref(), move || {
            let content_units = crate::file_meta::preserve_bom_units(
                prior_decoded.as_deref().unwrap_or(&[]),
                &requested_content,
            )
            .into_owned();
            let (structured_patch, lines_added, lines_removed) = if is_create {
                (crate::generated_result(&json!([])), 0, 0)
            } else {
                crate::utf16_mutation::patch(
                    prior_decoded.as_deref().unwrap_or(&[]),
                    &content_units,
                )
            };
            let bytes = crate::file_meta::encode_units_with_metadata(
                &content_units,
                encoding,
                crate::file_meta::LineEnding::Lf,
            );
            let published_units = crate::file_meta::normalize_to_lf_units(
                crate::utf16_mutation::without_bom(&content_units),
            );
            (
                prior_decoded,
                content_units,
                bytes,
                published_units,
                structured_patch,
                lines_added,
                lines_removed,
            )
        })
        .await?;
        let content_display = String::from_utf16_lossy(&content_units);
        let content = content_display.as_str();

        // (/rewind) Back up the pre-write content (or record a null backup for a
        // brand-new file) BEFORE writing so /rewind can restore it.
        if let Some(fh) = ctx.file_history.as_ref() {
            fh.track_edit(&canon.to_string_lossy()).await;
        }

        let bytes_written = bytes.len() as u64;
        let commit_path = path.clone();
        let commit_canon = canon.clone();
        let commit_requested = requested_path.clone();
        let commit_file_path = file_path.to_owned();
        let commit_scope = memory_mutation.clone();
        let state =
            tool_api::read_file_state::capture_read_file_state_write(&self.ctx.read_file_state);
        let history = ctx.file_history.clone();
        let result = crate::mutation_io::commit(ctx.cancel.as_ref(), move || {
            if let Some(blocked) = state.start_only_write_guard(&commit_requested, &commit_canon) {
                return Err(ToolError::InvalidInput(blocked.message().into()));
            }
            let result = if let Some(scope) = &commit_scope {
                scope.write_file_checked(&bytes, &write_expectation)
            } else {
                let Some((root, relative)) =
                    crate::shared::rooted_location(&commit_canon, &trusted_dirs)
                else {
                    return Err(ToolError::PathBlocked { path: commit_path });
                };
                lingxi_core::host::rooted_fs::write_file_after_permission_checked(
                    &root,
                    &relative,
                    &commit_path,
                    &commit_canon,
                    &bytes,
                    &write_expectation,
                )
            };
            let mut result =
                result.map_err(|error| write_resolution_error(&commit_file_path, error))?;
            if let (Some(history), Some(parent)) = (history.as_ref(), result.parent.take()) {
                history.track_completed_write(&commit_canon.to_string_lossy(), parent);
            }
            let new_mtime_ms = result
                .modified
                .map_or(0, tool_api::read_file_state::mtime_ms_floor);
            state.set_after_write_with_requested_aliases_and_units(
                commit_requested.clone(),
                tool_api::read_file_state::ReadFileEntry {
                    content: String::from_utf16_lossy(&published_units),
                    mtime_ms: new_mtime_ms,
                    offset: None,
                    limit: None,
                    from_read: false,
                    seeded_from_context: false,
                    is_partial_view: false,
                },
                true,
                vec![commit_requested, commit_path],
                Some(published_units),
            );
            Ok(result)
        })
        .await?;
        let _write_result = match result {
            Ok(result) => result,
            Err(error) => {
                self.emit_failed(&invocation_id, "io_write").await;
                return Err(error);
            }
        };

        if lines_added > 0 || lines_removed > 0 {
            // The oracle records the MODEL that made the change, not the tool.
            otel::record_lines_of_code_change(
                &ctx.options.main_loop_model,
                lines_added,
                lines_removed,
            );
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, bytes_written, duration_ms)
            .await;

        // Mirror Claude Code's automatic post-edit diagnostics path by
        // updating the matching LSP document before the next model turn.
        self.ctx.sync_lsp_after_file_write(&canon, content).await;

        // Model-facing result string is byte-faithful to claude-code
        // (`FileWriteTool.ts:418-433`); it echoes the ORIGINAL `file_path` arg,
        // not the canonicalized path. claude derives the model text from the
        // tool's structured result via `mapToolResultToToolResultBlockParam`;
        // LingXi surfaces the message verbatim via `ToolCallResult.model_content`
        // (the dispatch prefers that field over deriving text from `data`), so
        // `data` is PURE METADATA — the file payload, NOT the message.
        let content_message = write_result_message(file_path, is_create);
        let type_str = if is_create { "create" } else { "update" };

        // Pre-write content tracking → structured patch (`FileWriteTool.ts:359-376`):
        // an `update` (pre-existing non-empty file, `if (oldContent)`) emits the
        // jsdiff `structuredPatch` hunk array of the prior content → the new
        // content; a `create` emits `[]` (TS `structuredPatch: []`). Same 1:1
        // jsdiff port as Edit (`structured_patch::build_structured_patch`).
        // `data` is byte-faithful to claude-code's Write result `data` object —
        // the object `mapToolResultToToolResultBlockParam` receives (binary
        // field names, casing, and order):
        //   `{type, filePath, content, structuredPatch, originalFile,
        //     userModified, gitDiff?}`
        // from `S={type:…,filePath:e,content:t,structuredPatch:y,originalFile:h,
        //          userModified:s??!1,..._&&{gitDiff:_}}`.
        //   - `type`            = `create`|`update` (keyed on prior truthiness).
        //   - `filePath`        = the ORIGINAL `file_path` input (echoed
        //                          verbatim, NOT canonicalized).
        //   - `content`         = the FILE BYTES WRITTEN (the model-sent
        //                          `content`), NOT the result message. claude's
        //                          schema: "content that was written to the file".
        //   - `structuredPatch` = the jsdiff hunk array
        //                          `[{oldStart,oldLines,newStart,newLines,lines}]`
        //                          (1:1 port). `create` → `[]`.
        //   - `originalFile`    = the pre-write file content (`null` for a
        //                          create, the prior bytes for an update). claude
        //                          schema: nullable, "null for new files".
        //   - `userModified`    = `false` — the human-in-the-loop "modified your
        //                          proposed changes" accept step does not exist
        //                          in this non-interactive orchestrator.
        //   - `gitDiff`         = OMITTED (absent) — claude only attaches it when
        //                          git diff capture is enabled; LingXi has none.
        let original_file = if is_create {
            crate::generated_result(&Value::Null)
        } else {
            crate::utf16_mutation::string(prior_decoded.as_deref().unwrap_or(&[]))
        };
        let mut data = crate::generated_result(&json!({
            "type": type_str,
            "filePath": file_path,
            "content": "",
            "structuredPatch": [],
            "originalFile": null,
            "userModified": false,
        }));
        data.set_field("content", crate::utf16_mutation::string(&content_units))
            .expect("result field");
        data.set_field("structuredPatch", structured_patch)
            .expect("result field");
        data.set_field("originalFile", original_file)
            .expect("result field");
        Ok(ToolCallResult {
            data,
            // Model-facing text is the byte-faithful Write message, surfaced
            // verbatim (NOT a JSON dump of `data`).
            model_content: Some(content_message.into()),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_admission_rejects_unread_partial_and_start_only_before_reading_body() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("admission.txt");
        std::fs::write(&target, "unseen content").unwrap();
        for reason in ["missing", "partial", "start_only", "waived"] {
            let (ctx, _) = make_ctx(&tmp);
            let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
            if reason == "partial" {
                seed_full_read(&ctx, &target);
                let mut entry =
                    tool_api::read_file_state::get(&ctx.read_file_state, &route).unwrap();
                entry.is_partial_view = true;
                tool_api::read_file_state::set(&ctx.read_file_state, route.clone(), entry);
            } else if reason == "start_only" {
                tool_api::read_file_state::mark_start_only(&ctx.read_file_state, route.clone());
            }
            // Metadata is available, but this descriptor cannot read bytes.
            // Any attempt to load the body therefore produces Read(..), making
            // the admission-before-I/O assertion independent of file size/time.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .unwrap();
            let metadata = file.metadata().unwrap();
            let state =
                tool_api::read_file_state::capture_read_file_state_write(&ctx.read_file_state);
            let result = admitted_write_snapshot(
                Some((file, metadata)),
                &state,
                &route,
                &target,
                reason != "missing",
                None,
            );
            assert!(
                match (reason, result) {
                    ("missing", Err(WriteSnapshotError::MissingRead))
                    | ("partial", Err(WriteSnapshotError::PartialRead))
                    | ("start_only", Err(WriteSnapshotError::StartOnly(_)))
                    | ("waived", Err(WriteSnapshotError::Read(_))) => true,
                    _ => false,
                },
                "unexpected admission for {reason}"
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "unseen content");
        }
    }

    #[test]
    fn write_snapshot_rejects_growth_shrink_and_cancellation() {
        use lingxi_core::host::rooted_fs::RootedFsError;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("changing.txt");
        for replacement in [b"growing file".as_slice(), b"".as_slice()] {
            std::fs::write(&target, "old").unwrap();
            let file = std::fs::File::open(&target).unwrap();
            let metadata = file.metadata().unwrap();
            std::fs::write(&target, replacement).unwrap();
            assert!(matches!(
                read_write_snapshot(file, metadata, None),
                Err(RootedFsError::ContentChanged)
            ));
            assert_eq!(std::fs::read(&target).unwrap(), replacement);
        }
        let file = std::fs::File::open(&target).unwrap();
        let metadata = file.metadata().unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            read_write_snapshot(file, metadata, Some(&cancel)),
            Err(RootedFsError::Cancelled)
        ));
    }

    #[test]
    fn write_snapshot_checks_cancellation_between_bounded_chunks() {
        struct CancellingReader {
            cancel: tokio_util::sync::CancellationToken,
            reads: usize,
            bytes: usize,
        }
        impl std::io::Read for CancellingReader {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                self.bytes += bytes.len();
                bytes.fill(b'a');
                self.cancel.cancel();
                Ok(bytes.len())
            }
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut reader = CancellingReader {
            cancel: cancel.clone(),
            reads: 0,
            bytes: 0,
        };
        assert!(matches!(
            read_write_snapshot_bytes(&mut reader, 1024 * 1024, Some(&cancel)),
            Err(lingxi_core::host::rooted_fs::RootedFsError::Cancelled)
        ));
        assert_eq!(reader.reads, 1);
        assert_eq!(reader.bytes, 64 * 1024);

        // Even a reader that never reaches EOF is stopped at the descriptor's
        // admitted length plus one growth sentinel.
        let mut growing = std::io::repeat(b'a');
        assert!(matches!(
            read_write_snapshot_bytes(&mut growing, 3, None),
            Err(lingxi_core::host::rooted_fs::RootedFsError::ContentChanged)
        ));
    }

    #[test]
    fn admitted_write_snapshot_retains_eligible_large_files() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("large.txt");
        let contents = vec![b'a'; 1024 * 1024 + 17];
        std::fs::write(&target, &contents).unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let file = std::fs::File::open(&target).unwrap();
        let metadata = file.metadata().unwrap();
        let state = tool_api::read_file_state::capture_read_file_state_write(&ctx.read_file_state);
        let snapshot =
            admitted_write_snapshot(Some((file, metadata)), &state, &route, &target, true, None)
                .unwrap()
                .unwrap();
        assert_eq!(snapshot.bytes, contents);
        assert_eq!(snapshot.size, contents.len() as u64);
    }

    fn native_322_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/file_mutation_322.json"
        ))
        .unwrap()
    }

    fn native_322_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn native_322_writes_preserve_encoding_bom_and_exact_content_bytes() {
        for case in native_322_fixture()["writes"].as_array().unwrap() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("encoded.txt");
            std::fs::write(
                &target,
                native_322_hex(case["originalHex"].as_str().unwrap()),
            )
            .unwrap();
            let (ctx, _) = make_ctx(&tmp);
            seed_full_read(&ctx, &target);
            let tool = FileWriteTool::new(ctx);
            let result = tool
                .call(
                    crate::generated_result(&json!({
                        "file_path": target.to_str().unwrap(), "content": case["replacement"],
                    })),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap();
            let expected = &case["expected"];
            assert_eq!(
                std::fs::read(&target).unwrap(),
                native_322_hex(expected["bytesHex"].as_str().unwrap()),
                "{}",
                case["name"]
            );
            let data = crate::test_data(&result.data);
            assert_eq!(data["content"], expected["content"], "{}", case["name"]);
            assert_eq!(
                data["originalFile"], expected["originalFile"],
                "{}",
                case["name"]
            );
        }
    }

    #[tokio::test]
    async fn native_322_write_refuses_unsupported_encoding_without_changing_bytes() {
        for bytes in [
            vec![0xfe, 0xff, 0, b'a'],
            vec![b'a', 0, b'b'],
            vec![b'c', b'a', b'f', 0xe9],
        ] {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("encoded.txt");
            std::fs::write(&target, &bytes).unwrap();
            let (ctx, _) = make_ctx(&tmp);
            seed_full_read(&ctx, &target);
            let tool = FileWriteTool::new(ctx);
            let err = tool
                .call(
                    crate::generated_result(&json!({
                        "file_path": target.to_str().unwrap(), "content": "replacement",
                    })),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap_err();
            let expected = crate::file_meta::classify_not_text(&bytes)
                .unwrap()
                .mutation_refusal_message();
            assert!(
                matches!(err, ToolError::InvalidInput(message) if message.string_at("") == Some(expected.as_str()))
            );
            assert_eq!(std::fs::read(&target).unwrap(), bytes);
        }
    }

    #[tokio::test]
    async fn native_322_partial_read_never_authorizes_write_at_unchanged_mtime() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("partial.txt");
        std::fs::write(&target, "unseen content").unwrap();
        let (ctx, _) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let mut entry = tool_api::read_file_state::get(&ctx.read_file_state, &route).unwrap();
        entry.is_partial_view = true;
        tool_api::read_file_state::set(&ctx.read_file_state, route, entry);
        let tool = FileWriteTool::new(ctx);
        let err = tool
            .call(
                crate::generated_result(&json!({
                    "file_path": target.to_str().unwrap(), "content": "replacement",
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidInput(message) if message.string_at("") == Some(crate::FILE_NOT_READ_ERROR))
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "unseen content");
    }

    #[tokio::test]
    async fn native_322_write_directory_error_bytes() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let err = tool
            .call(
                crate::generated_result(&json!({
                    "file_path": target.to_str().unwrap(), "content": "replacement",
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let fixture = native_322_fixture();
        let template = fixture["writeTargets"][0]["expected"]["message"]
            .as_str()
            .unwrap();
        let expected = template.replace("/oracle/target", target.to_str().unwrap());
        assert!(
            matches!(err, ToolError::InvalidInput(message) if message.string_at("") == Some(expected.as_str()))
        );
        assert!(target.is_dir());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_322_write_socket_error_bytes() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        let _socket = std::os::unix::net::UnixListener::bind(&target).unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let err = tool
            .call(
                crate::generated_result(&json!({
                    "file_path": target.to_str().unwrap(), "content": "replacement",
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let fixture = native_322_fixture();
        let template = fixture["writeTargets"][1]["expected"]["message"]
            .as_str()
            .unwrap();
        let expected = template.replace("/oracle/target", target.to_str().unwrap());
        assert!(
            matches!(err, ToolError::InvalidInput(message) if message.string_at("") == Some(expected.as_str()))
        );
        assert!(std::fs::symlink_metadata(&target).is_ok());
    }
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tempfile::TempDir;
    use tool_api::test_support::{fresh_ctx, fresh_tx, make_dummy_fs};

    fn make_ctx(tmp: &TempDir) -> (BuiltinToolContext, Arc<InMemorySink>) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        (
            tool_api::test_support::ctx_for_file_tools(
                make_dummy_fs(),
                bus,
                vec![tmp.path().to_path_buf()],
            ),
            sink,
        )
    }

    /// Simulate a prior full `Read` of `target` so the read-before-write
    /// staleness guard (Batch F) is satisfied when overwriting an EXISTING
    /// file: records the file's current raw-UTF-8 content + floored mtime under
    /// Native's normalized model-route key with `offset`/`limit` = `None`.
    /// Canonical remains the source for disk access only. Call AFTER writing
    /// the file so the seeded mtime matches the on-disk mtime.
    fn seed_full_read(ctx: &BuiltinToolContext, target: &std::path::Path) {
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let canon = std::fs::canonicalize(target).unwrap();
        let bytes = std::fs::read(&canon).unwrap();
        let decoded = crate::file_meta::read_with_metadata(&bytes).0;
        let content = decoded
            .strip_prefix('\u{feff}')
            .unwrap_or(&decoded)
            .to_owned();
        let mtime_ms = std::fs::metadata(&canon)
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        tool_api::read_file_state::set(
            &ctx.read_file_state,
            route,
            tool_api::read_file_state::ReadFileEntry {
                content,
                mtime_ms,
                offset: None,
                limit: None,
                // Simulates a prior full `Read`.
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
    }

    #[tokio::test]
    async fn actual_read_then_write_uses_the_normalized_session_route_key() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        let target = tmp.path().join("source.txt");
        std::fs::write(&target, "old text\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let state = ctx.read_file_state.clone();
        let input_path = "nested/../source.txt";
        let route = crate::normalize_model_file_path(input_path, &ctx.cwd());

        crate::read::FileReadTool::new(ctx.clone())
            .call(
                crate::generated_result(&json!({ "file_path": input_path })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Read succeeds through the active session cwd");
        assert!(tool_api::read_file_state::get(&state, &route).is_some());

        FileWriteTool::new(ctx)
            .call(
                crate::generated_result(
                    &json!({ "file_path": input_path, "content": "new text\n" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Write looks up the same Native lexical route that Read recorded");
        assert_eq!(std::fs::read_to_string(target).unwrap(), "new text\n");
    }

    #[test]
    fn tool_name_is_write() {
        assert_eq!(TOOL_NAME, "Write");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn leaf_symlink_is_refused_without_touching_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&target, "original").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        crate::read::FileReadTool::new(ctx.clone())
            .call(
                crate::generated_result(&json!({ "file_path": link.to_string_lossy() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Read the same lexical route before testing the write refusal");
        let tool = FileWriteTool::new(ctx);

        let error = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": link.to_string_lossy(), "content": "changed" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("Write must refuse a final symlink");
        let message = match error {
            tool_api::tool_trait::ToolError::InvalidInput(message) => message,
            other => panic!("unexpected error: {other:?}"),
        };
        let message = message.string_at("").unwrap();
        assert!(message.starts_with("Refusing to write "));
        assert!(
            message.ends_with("it is a symbolic link. Write to the link's target path instead.")
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn write_resolution_messages_are_byte_exact() {
        for (reason, expected) in [
            (lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink,
             "Refusing to write /tmp/link.txt: it is a symbolic link. Write to the link's target path instead."),
            (lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged,
             "Refusing to write /tmp/link.txt: its parent-directory symlink resolution changed after permission was checked."),
        ] {
            let error = write_resolution_error("/tmp/link.txt", reason);
            assert!(matches!(error, ToolError::InvalidInput(_)));
            assert_eq!(error.model_facing_message().string_units(""),
                       Some(expected.encode_utf16().collect::<Vec<_>>()));
        }
    }

    /// S2 (PathAtlas): a Write to a guest path lands the bytes in the
    /// host-backed twin directory.
    #[tokio::test]
    async fn guest_path_write_lands_on_the_host_twin() {
        let host = tempfile::TempDir::new().unwrap();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_guest_alias_fs("/workspace/abc", host.path(), "/fenced"),
            std::sync::Arc::new(telemetry::AnalyticsBus::new()),
            vec![host.path().to_path_buf()],
        );
        let tool = FileWriteTool::new(ctx);
        tool.call(
            crate::generated_result(&serde_json::json!({ "file_path": "/workspace/abc/new.txt", "content": "from guest" })),
            tool_api::test_support::fresh_ctx(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
        let written = std::fs::read_to_string(host.path().join("new.txt")).unwrap();
        assert_eq!(written, "from guest");
    }

    #[tokio::test]
    async fn prompt_is_model_gated() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        // model:claude-sonnet-4-6 selects the native long template (byte-anchor).
        let long_prompt = tool
            .prompt(&PromptOptions {
                prompt_policy: None,
                is_scheduled_turn: false,
                session_id: None,
                include_examples: false,
                model: Some("claude-sonnet-4-6".into()),
                model_profile: None,
                ..Default::default()
            })
            .await;
        let long = long_prompt.string_at("").expect("prompt is a JSON string");
        assert_eq!(long, WRITE_PROMPT_LONG);
        assert!(long.starts_with("Writes a file to the local filesystem.\n\nUsage:"));
        assert!(long.contains("you MUST use the Read tool first"));
        // model:claude-opus-4-8 ⇒ Dh=true ⇒ SHORT (byte-anchor).
        let short_prompt = tool
            .prompt(&PromptOptions {
                prompt_policy: None,
                is_scheduled_turn: false,
                session_id: None,
                include_examples: false,
                model: Some("claude-opus-4-8".to_string()),
                model_profile: None,
                ..Default::default()
            })
            .await;
        let short = short_prompt.string_at("").expect("prompt is a JSON string");
        assert_eq!(short, WRITE_PROMPT_SHORT);
        assert!(
            short.starts_with("Writes a file to the local filesystem, overwriting if one exists.")
        );
        assert!(short.contains("For partial changes, use Edit instead."));
    }

    #[test]
    fn write_result_message_create_is_byte_locked() {
        // FileWriteTool.ts:420-425.
        assert_eq!(
            write_result_message("/tmp/new.txt", true),
            "File created successfully at: /tmp/new.txt (file state is current in your context — no need to Read it back)"
        );
    }

    #[test]
    fn write_result_message_update_is_byte_locked() {
        // FileWriteTool.ts:426-431 + the file-state-current suffix (`Pyn`).
        assert_eq!(
            write_result_message("/tmp/old.txt", false),
            "The file /tmp/old.txt has been updated successfully. (file state is current in your context — no need to Read it back)"
        );
    }

    #[test]
    fn write_result_message_echoes_original_path_verbatim() {
        assert_eq!(
            write_result_message("./a/../b.txt", true),
            "File created successfully at: ./a/../b.txt (file state is current in your context — no need to Read it back)"
        );
    }

    /// Worktree parity plan (Task 2) INERT INVARIANT: `BuiltinToolContext`
    /// now carries `session_cwd: Arc<SessionCwd>` instead of frozen
    /// `workspace`/`trusted_dirs` fields, and `Write`'s two
    /// `canonicalize_and_validate` sites (the parent-ancestor probe + the
    /// final target check) share a single `self.ctx.trusted_dirs()` snapshot.
    /// With nothing ever calling `session_cwd.swap(..)`, `ctx.cwd()` must
    /// equal the boot cwd, and a write must succeed exactly as it did before
    /// the migration.
    #[tokio::test]
    async fn no_swap_is_identical() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("out.txt");
        let (ctx, _sink) = make_ctx(&tmp);

        // No `session_cwd.swap(..)` call anywhere in this test.
        assert_eq!(ctx.cwd(), tmp.path());
        assert_eq!(ctx.trusted_dirs(), vec![tmp.path().to_path_buf()]);

        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "hello" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    }

    #[tokio::test]
    async fn happy_path_writes_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("out.txt");
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "hello" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // New file → `create`. `data.content` is the FILE BYTES written (NOT
        // the message); the model message is surfaced verbatim via
        // `model_content` and echoes the ORIGINAL input path.
        let input_path = target.to_str().unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert_eq!(crate::test_data(&result.data)["filePath"], input_path);
        assert_eq!(crate::test_data(&result.data)["content"], "hello");
        assert_eq!(crate::test_data(&result.data)["originalFile"], Value::Null);
        assert_eq!(crate::test_data(&result.data)["userModified"], false);
        assert!(crate::test_data(&result.data)
            .get("bytes_written")
            .is_none());
        assert!(crate::test_data(&result.data).get("gitDiff").is_none());
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
            write_result_message(input_path, true)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"tengu_tool_write_started".to_string()));
        assert!(names.contains(&"tengu_tool_write_completed".to_string()));
    }

    #[tokio::test]
    async fn subagent_report_md_write_is_blocked() {
        // A subagent (ctx.agent_id = Some) writing REPORT*.md is hard-blocked
        // with CC's exact message; a same-named write by the MAIN agent
        // (agent_id = None) is allowed.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("REPORT-final.md");
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileWriteTool::new(ctx);

        let mut subagent_ctx = fresh_ctx();
        subagent_ctx.agent_id = Some(lingxi_core::types::AgentId::new());
        let err = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "findings" }),
                ),
                subagent_ctx,
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => assert_eq!(
                m.string_at(""),
                Some(
                    "Subagents should return findings as text, not write report files. Include this content in your final response instead."
                )
            ),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        assert!(!target.exists(), "blocked write must not create the file");
        let names: Vec<String> = sink.events().await.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"tengu_subagent_md_report_blocked".to_string()));

        // Main agent (agent_id None) is unaffected.
        let (ctx2, _s2) = make_ctx(&tmp);
        let tool2 = FileWriteTool::new(ctx2);
        tool2
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "ok" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert!(target.exists());
    }

    #[test]
    fn is_subagent_report_md_matches_cc_regex() {
        use std::path::Path;
        // `/^(REPORT|SUMMARY|FINDINGS|ANALYSIS).*\.md$/i`
        for p in [
            "REPORT.md",
            "report-v2.md",
            "Summary_final.md",
            "FINDINGS.MD",
            "analysis.md",
        ] {
            assert!(is_subagent_report_md(Path::new(p)), "{p} should match");
        }
        for p in [
            "notes.md",
            "report.txt",
            "my-report.md",
            "readme.md",
            "REPORT",
        ] {
            assert!(!is_subagent_report_md(Path::new(p)), "{p} must NOT match");
        }
    }

    #[tokio::test]
    async fn create_emits_empty_patch_preview() {
        // New file → `create` → empty patch (TS `structuredPatch: []`).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("created.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "hello\nworld\n" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        // create → `structuredPatch: []` (empty hunk array, TS parity).
        assert_eq!(crate::test_data(&result.data)["structuredPatch"], json!([]));
        // `create` → originalFile is null (TS "null for new files").
        assert_eq!(crate::test_data(&result.data)["originalFile"], Value::Null);
        // `data.content` is the FILE BYTES written, not the message.
        assert_eq!(crate::test_data(&result.data)["content"], "hello\nworld\n");
    }

    #[tokio::test]
    async fn update_emits_diff_patch_preview_from_prior_content() {
        // Pre-existing non-empty file → `update` → patch diffs prior → new.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("upd.txt");
        std::fs::write(&target, "alpha\nbeta\ngamma\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap(), "content": "alpha\nBETA\ngamma\n" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "update");
        // `originalFile` is the pre-write content; `content` is the new bytes.
        assert_eq!(
            crate::test_data(&result.data)["originalFile"],
            "alpha\nbeta\ngamma\n"
        );
        assert_eq!(
            crate::test_data(&result.data)["content"],
            "alpha\nBETA\ngamma\n"
        );
        // The structuredPatch is the jsdiff hunk array capturing the changed
        // middle line (1:1: context + `-beta`/`+BETA` + context).
        let data = crate::test_data(&result.data);
        let sp = data["structuredPatch"].as_array().expect("hunk array");
        assert_eq!(sp.len(), 1, "one hunk");
        assert_eq!(sp[0]["oldStart"], 1);
        assert_eq!(sp[0]["oldLines"], 3);
        assert_eq!(sp[0]["newStart"], 1);
        assert_eq!(sp[0]["newLines"], 3);
        let lines: Vec<&str> = sp[0]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap())
            .collect();
        assert_eq!(lines, vec![" alpha", "-beta", "+BETA", " gamma"]);
    }

    #[tokio::test]
    async fn creates_missing_parent_unconditionally() {
        // claude-code creates parents unconditionally (`FileWriteTool.ts:254`):
        // a missing parent inside the trusted dirs is created, not rejected.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub").join("deep").join("out.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "hi" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert_eq!(crate::test_data(&result.data)["content"], "hi");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
    }

    #[tokio::test]
    async fn creates_parent_no_flag_needed() {
        // Same as above but a deeper tree and longer content — no `mkdir` flag.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub").join("deep").join("out.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(&json!({
                    "file_path": target.to_str().unwrap(),
                    "content": "deep content"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["content"], "deep content");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "deep content");
    }

    #[tokio::test]
    async fn writes_nested_path_with_no_flags() {
        // `a/b/c.txt` under the trusted dir succeeds with only {file_path,
        // content} and no flags — all intermediate dirs are created.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a").join("b").join("c.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "nested" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert!(target.exists());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "nested");
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("over.txt");
        std::fs::write(&target, "old").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // Overwriting an existing file requires a prior full Read (Batch F).
        seed_full_read(&ctx, &target);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "new" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Pre-existing non-empty file → `update`: byte-faithful message lives on
        // `model_content`; `data.content` is the new FILE BYTES.
        let input_path = target.to_str().unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "update");
        assert_eq!(crate::test_data(&result.data)["filePath"], input_path);
        assert_eq!(crate::test_data(&result.data)["content"], "new");
        assert_eq!(crate::test_data(&result.data)["originalFile"], "old");
        assert_eq!(crate::test_data(&result.data)["userModified"], false);
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
            write_result_message(input_path, false)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[tokio::test]
    async fn overwriting_empty_file_is_treated_as_create() {
        // claude-code keys `type` on `if (oldContent)` truthiness: an existing
        // but EMPTY file is falsy and yields `create` (FileWriteTool.ts:359).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("empty.txt");
        std::fs::write(&target, "").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // An existing-but-empty file still physically exists, so the guard
        // applies even though create-vs-update treats it as `create`.
        seed_full_read(&ctx, &target);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "filled" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let input_path = target.to_str().unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        // create → originalFile is null even for an existing-but-empty file
        // (TS `if (oldContent)` is falsy → create branch, `originalFile:null`).
        assert_eq!(crate::test_data(&result.data)["originalFile"], Value::Null);
        assert_eq!(crate::test_data(&result.data)["content"], "filled");
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
            write_result_message(input_path, true)
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "filled");
    }

    #[tokio::test]
    async fn new_file_in_created_parent_is_create() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("sub").join("deep").join("out.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(&json!({
                    "file_path": target.to_str().unwrap(),
                    "content": "deep content"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let input_path = target.to_str().unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert_eq!(crate::test_data(&result.data)["content"], "deep content");
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
            write_result_message(input_path, true)
        );
    }

    // ───────────────────────── Batch F: staleness guard ─────────────────────

    #[tokio::test]
    async fn write_without_prior_read_errors_not_read() {
        // Overwriting an existing file with NO recorded Read → FILE_NOT_READ_ERROR.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("guarded.txt");
        std::fs::write(&target, "existing content").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // Deliberately do NOT seed a prior read.
        let tool = FileWriteTool::new(ctx);
        let err = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "new" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => {
                assert_eq!(m.string_at(""), Some(crate::FILE_NOT_READ_ERROR));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "existing content"
        );
    }

    #[tokio::test]
    async fn new_file_write_skips_guard() {
        // Writing a NONEXISTENT path is creation → the guard is skipped even
        // without a prior Read (TS `meta === null`).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("fresh.txt");
        assert!(!target.exists());
        let (ctx, _sink) = make_ctx(&tmp);
        // No seed; the file does not exist, so the guard must not apply.
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "brand new" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "brand new");
    }

    #[tokio::test]
    async fn external_modify_then_write_errors_modified() {
        // Read → external modify (mtime bumps AND content changes) → Write must
        // refuse with FILE_UNEXPECTEDLY_MODIFIED_ERROR.
        use filetime::{set_file_mtime, FileTime};
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("ext.txt");
        std::fs::write(&target, "original").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        // External actor rewrites the file AND bumps mtime forward.
        std::fs::write(&target, "tampered").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        let tool = FileWriteTool::new(ctx);
        let err = tool
            .call(
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap(), "content": "model wrote this" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            // FT-07: stale-changed-content returns the oracle's
            // `validateInput` errorCode-3 literal (cc-238.js @226416137),
            // not the call-phase race sentence `WVo`.
            ToolError::InvalidInput(m) => {
                assert_eq!(
                    m.string_at(""),
                    Some(crate::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // Write refused → file left as the external content.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "tampered");
    }

    #[tokio::test]
    async fn same_content_touch_then_write_proceeds_via_fallback() {
        // Read(full) → mtime bumped but bytes UNCHANGED → Write proceeds via the
        // content-equality fallback.
        use filetime::{set_file_mtime, FileTime};
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("touched.txt");
        std::fs::write(&target, "keep").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        // Touch: bump mtime forward WITHOUT changing content.
        set_file_mtime(&target, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "overwritten" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Pre-existing non-empty file → update.
        assert_eq!(crate::test_data(&result.data)["type"], "update");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "overwritten");
    }

    #[tokio::test]
    async fn post_write_set_lets_immediate_second_write_succeed() {
        // First Write succeeds with a seeded read; its post-write `set` updates
        // the registry so a SECOND immediate Write (no re-seed) also succeeds.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("twice.txt");
        std::fs::write(&target, "v0").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = FileWriteTool::new(ctx);
        tool.call(
            crate::generated_result(
                &json!({ "file_path": target.to_str().unwrap(), "content": "v1" }),
            ),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // No re-seed: the post-write `set` from the first write must satisfy
        // the guard for the second write.
        tool.call(
            crate::generated_result(
                &json!({ "file_path": target.to_str().unwrap(), "content": "v2" }),
            ),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2");
    }

    #[tokio::test]
    async fn rejects_path_outside_trusted() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("a.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let err = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "content": "x" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::PathBlocked { .. } => {}
            other => panic!("expected PathBlocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn is_destructive_returns_true() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        assert!(tool.is_destructive(&crate::generated_result(&json!({}))));
    }

    #[tokio::test]
    async fn is_concurrency_safe_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        assert!(!tool.is_concurrency_safe(&crate::generated_result(&json!({}))));
    }

    #[tokio::test]
    async fn is_read_only_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        assert!(!tool.is_read_only(&crate::generated_result(&json!({}))));
    }

    #[tokio::test]
    async fn input_schema_requires_file_path_and_content() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let schema = tool.input_schema();
        assert_eq!(schema.array_len("/required"), Some(2));
        assert_eq!(schema.string_at("/required/0"), Some("file_path"));
        assert_eq!(schema.string_at("/required/1"), Some("content"));
        // claude-code schema is {file_path, content} ONLY — no `mkdir`.
        assert!(
            !schema.contains_property("mkdir") && schema.at_pointer("/properties/mkdir").is_none(),
            "schema must NOT expose `mkdir`"
        );
    }

    /// Worktree parity plan (Task 3): a RELATIVE `file_path` must resolve
    /// against the CURRENT `session_cwd` — so after `EnterWorktree` swaps the
    /// cwd into a worktree, a relative-path `Write` lands under the
    /// worktree, NOT the boot cwd (which is what `std::fs::canonicalize`
    /// would otherwise resolve a relative path against, since it consults
    /// the frozen OS process cwd, not the switchable session cwd).
    #[tokio::test]
    async fn relative_path_write_follows_session_cwd_swap_into_worktree() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);

        // Swap the session cwd into the worktree subdir (trusting it too),
        // the way `EnterWorktree` does.
        ctx.session_cwd.swap(wt.clone(), vec![wt.clone()]);
        assert_eq!(ctx.cwd(), wt);

        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": "relative.txt", "content": "hello" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert_eq!(
            std::fs::read_to_string(wt.join("relative.txt")).unwrap(),
            "hello",
            "relative file_path must resolve under the SWAPPED worktree cwd"
        );
        assert!(
            !tmp.path().join("relative.txt").exists(),
            "must NOT have landed under the boot cwd"
        );
    }

    /// INERT INVARIANT companion: with no `session_cwd.swap(..)` call, a
    /// relative `file_path` resolves under the boot cwd exactly as it did
    /// before this task's fix.
    #[tokio::test]
    async fn relative_path_write_resolves_under_boot_cwd_without_swap() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        assert_eq!(ctx.cwd(), tmp.path(), "no swap happened in this test");

        let tool = FileWriteTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": "relative.txt", "content": "hello" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "create");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("relative.txt")).unwrap(),
            "hello"
        );
    }
    struct PlanFileOwner(String);
    #[async_trait]
    impl lingxi_core::host::teammate_plan::TeammatePlanRequester for PlanFileOwner {
        fn writable_plan_path(&self) -> Option<&str> {
            Some(&self.0)
        }
        async fn submit(
            &self,
            _: Utf16JsonProjection,
        ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
            unreachable!()
        }
    }
    #[tokio::test]
    async fn teammate_writes_only_own_plan_outside_workspace_and_rejects_symlink() {
        let workspace = TempDir::new().unwrap();
        let plans = TempDir::new().unwrap();
        let root = plans.path().canonicalize().unwrap();
        let target = root.join("own-plan.md");
        let sibling = root.join("sibling.md");
        let (ctx, _) = make_ctx(&workspace);
        let tool = FileWriteTool::new(ctx);
        let agent = lingxi_core::types::AgentId::new();
        let owner: Arc<dyn lingxi_core::host::teammate_plan::TeammatePlanRequester> =
            Arc::new(PlanFileOwner(target.to_string_lossy().into_owned()));
        lingxi_core::host::teammate_plan::register(agent, &owner);
        let context = || {
            let mut ctx = fresh_ctx();
            ctx.agent_id = Some(agent);
            ctx
        };
        tool.call(
            crate::generated_result(&json!({"file_path":target,"content":"approved scope"})),
            context(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved scope");
        assert!(tool
            .call(
                crate::generated_result(&json!({"file_path":sibling,"content":"not allowed"})),
                context(),
                fresh_tx()
            )
            .await
            .is_err());
        assert!(!sibling.exists());
        #[cfg(unix)]
        {
            std::fs::remove_file(&target).unwrap();
            std::fs::write(&sibling, "protected").unwrap();
            std::os::unix::fs::symlink(&sibling, &target).unwrap();
            assert!(tool
                .call(
                    crate::generated_result(&json!({"file_path":target,"content":"no"})),
                    context(),
                    fresh_tx()
                )
                .await
                .is_err());
            assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "protected");
        }
    }
}
