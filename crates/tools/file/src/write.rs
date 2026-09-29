//! `FileWriteTool` — write a UTF-8 file inside the trusted-dirs whitelist.
//!
//! Behaviour (1:1 with claude-code `FileWriteTool.ts`):
//! - The schema is `{file_path, content}` only — there is NO `mkdir` flag.
//! - Parent directories are created UNCONDITIONALLY before the write
//!   (`FileWriteTool.ts:254` `mkdir(dir, recursive)`).
//! - Overwrite is allowed unconditionally.
//! - Content is UTF-8; no BOM is written.

use crate::structured_patch::count_patch_lines;
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
    canonicalize_and_validate, emit_blocked_event, resolve_against_cwd, translate_model_path,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock — matches claude-code tool registry.
pub const TOOL_NAME: &str = "Write";

/// The LONG Write prompt — byte-locked VERBATIM to claude-code `v0i(e)`'s
/// `Dh(e)===false` branch (binary offset ~196432410): the base text plus
/// `${ZAd()}` (the read-first line, `${Ws}`=Read) appended to the overwrite
/// bullet, spelled out inline here. `${Ua}`=Edit. Em-dash is U+2014.
const WRITE_PROMPT_LONG: &str = "Writes a file to the local filesystem.\n\nUsage:\n\
- This tool will overwrite the existing file if there is one at the provided path.\n\
- If this is an existing file, you MUST use the Read tool first to read the file's contents. This tool will fail if you did not read the file first.\n\
- Prefer the Edit tool for modifying existing files \u{2014} it only sends the diff. Only use this tool to create new files or for complete rewrites.\n\
- NEVER create documentation files (*.md) or README files unless explicitly requested by the User.\n\
- Only use emojis if the user explicitly requests it. Avoid writing emojis to files unless asked.";

/// The SHORT Write prompt — byte-locked VERBATIM to claude-code `v0i(e)`'s
/// `Dh(e)===true` branch (binary offset 196432581), served to current-gen
/// default models. `${Ws}`=Read, `${Ua}`=Edit. Em-dash is U+2014.
const WRITE_PROMPT_SHORT: &str = "Writes a file to the local filesystem, overwriting if one exists.\n\
\n\
When to use: creating a new file, or fully replacing one you've already Read. Overwriting an existing file you haven't Read will fail. For partial changes, use Edit instead.";

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

fn write_rooted_snapshot(
    requested: &std::path::Path,
    approved: &std::path::Path,
    trusted_dirs: &[std::path::PathBuf],
) -> Result<
    lingxi_core::host::rooted_fs::RootedFileSnapshot,
    lingxi_core::host::rooted_fs::RootedFsError,
> {
    let Some((root, relative)) = crate::shared::rooted_location(approved, trusted_dirs) else {
        return Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
            lingxi_core::host::FsError::OutsideWorkspace(approved.display().to_string()),
        ));
    };
    lingxi_core::host::rooted_fs::read_file_after_permission(&root, &relative, requested, approved)
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
        lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink => ToolError::InvalidInput(format!(
            "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
        )),
        lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged => {
            ToolError::InvalidInput(format!(
                "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
            ))
        }
        lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged => {
            if std::fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
                ))
            } else {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
                ))
            }
        }
        lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile => {
            ToolError::Io(format!("File {path} is not a regular file"))
        }
        lingxi_core::host::rooted_fs::RootedFsError::Fs(error) => ToolError::Io(error.to_string()),
    }
}

impl FileWriteTool {
    /// Construct a new tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn emit_started(&self, invocation_id: &str, path: &std::path::Path) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "_PROTO_file_path".to_string(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(path.display().to_string()).into_inner(),
            ),
        );
        self.ctx.bus.log_event(WRITE_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, bytes_written: u64, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "bytes_written".to_string(),
            AnalyticsValue::Int(bytes_written as i64),
        );
        md.insert(
            "duration_ms".to_string(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(WRITE_COMPLETED, md).await;
    }

    async fn emit_failed(&self, invocation_id: &str, kind: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "failure_kind".to_string(),
            AnalyticsValue::String(Verified::assert_safe(kind.to_string()).into_inner()),
        );
        self.ctx.bus.log_event(WRITE_FAILED, md).await;
    }

    /// Emit `tengu_subagent_md_report_blocked` with the attempted content's
    /// UTF-8 byte length (claude-code `Buffer.byteLength(t)`).
    async fn emit_subagent_md_report_blocked(&self, content: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "contentBytes".to_string(),
            AnalyticsValue::Int(i64::try_from(content.len()).unwrap_or(i64::MAX)),
        );
        self.ctx
            .bus
            .log_event(telemetry::tengu::tool::SUBAGENT_MD_REPORT_BLOCKED, md)
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

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["file_path", "content"],
        "properties": {
            "file_path": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)" },
            "content":   { "type": "string", "description": "The content to write to the file" }
        }
    })
});

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<&str> {
        Some("create or overwrite files")
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        // claude-code `FileWriteTool.ts:100`.
        "Write a file to the local filesystem.".to_string()
    }

    async fn prompt(&self, opts: &PromptOptions) -> String {
        // Model-gated, mirroring claude-code `v0i(e){if(Dh(e))return SHORT;
        // return LONG}` (binary offset ~196432410). Predicate shared with
        // TodoWrite via `tool_api`.
        if tool_api::dh_simple_system_prompt(opts.model.as_deref()) {
            WRITE_PROMPT_SHORT.to_string()
        } else {
            WRITE_PROMPT_LONG.to_string()
        }
    }

    fn get_path(&self, input: &Value) -> Option<PathBuf> {
        input
            .get("file_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = tool_api::util::ids::ulid_or_uuid();
        let file_path = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("file_path is required".into()))?;
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("content is required".into()))?;
        // Worktree parity plan (Task 3): a RELATIVE `file_path` resolves
        // against the CURRENT session cwd (`ctx.cwd()`, switchable by
        // `EnterWorktree`/`ExitWorktree`), not the frozen OS process cwd that
        // `std::fs::canonicalize` would otherwise consult below. An absolute
        // `file_path` (the documented/expected case) is unaffected.
        let path = resolve_against_cwd(PathBuf::from(file_path), &self.ctx.cwd());
        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, path, true) {
            Ok(path) => path,
            Err(message) => return Err(ToolError::InvalidInput(message)),
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

        let started = Instant::now();
        self.emit_started(&invocation_id, &path).await;

        // One snapshot of trusted_dirs for both containment checks below (the
        // parent-ancestor probe and the final target check), so they can't
        // straddle a swap between the two calls.
        let mut trusted_dirs = self.ctx.trusted_dirs();
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
        let canon = match canonicalize_write_target(&path, &trusted_dirs) {
            Ok(p) => p,
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::PathBlocked { path });
            }
        };

        // Determine create-vs-update BEFORE the write. claude-code keys the
        // result `type` on `if (oldContent)` (`FileWriteTool.ts:359`), a JS
        // truthiness check on the pre-write file contents: a missing file OR an
        // existing-but-empty file is `create`; only a pre-existing file with
        // non-empty content is `update`. We mirror that: read the prior bytes
        // non-fatally (a read error is treated as "no prior content" so it
        // falls through to `create`, matching the ENOENT→null path).
        //
        // `file_exists` separately tracks whether the file is physically
        // present on disk: the read-before-write staleness guard (Batch F) keys
        // on physical existence (TS `meta !== null`, `FileWriteTool.ts:279`),
        // NOT on the create-vs-update truthiness — an existing-but-empty file
        // still requires a prior Read.
        let prior_snapshot = match write_rooted_snapshot(&path, &canon, &trusted_dirs) {
            Ok(snapshot) => Some(snapshot),
            Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
                lingxi_core::host::FsError::NotFound(_),
            )) => None,
            Err(error @ lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink)
            | Err(
                error @ lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged,
            )
            | Err(error @ lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged) => {
                self.emit_failed(&invocation_id, "symlink_resolution_changed")
                    .await;
                return Err(write_resolution_error(file_path, error));
            }
            // A non-ENOENT read error means the path exists but is unreadable;
            // treat as "no prior content" and let the write operation report
            // its own error, matching the pre-hardening behavior.
            Err(_) => None,
        };
        let prior_mtime_ms = prior_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.modified)
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        let (is_create, file_exists, prior_decoded) = match prior_snapshot {
            Some(snapshot) => {
                // Raw UTF-8 decode matching how `Read` records content (used by
                // the content-equality fallback); a non-UTF-8 prior file leaves
                // it `None` and the mtime check alone governs.
                let decoded = crate::shared::decode_utf8_strict(&snapshot.bytes).ok();
                (snapshot.bytes.is_empty(), true, decoded)
            }
            None => (true, false, None),
        };

        // Read-before-write staleness guard (Batch F): only for an EXISTING
        // file. New-file creation skips it (TS `meta === null` skips the
        // guard; `FileWriteTool.ts:198-219` & `:279-295`).
        if file_exists {
            // Current mtime (floored ms), captured from the same opened handle
            // as the pre-write bytes; `None` falls back to epoch `0`.
            let current_mtime_ms = prior_mtime_ms;
            let cmp_content = prior_decoded.as_deref().unwrap_or("");
            if let Err(e) = crate::check_read_before_write(
                &self.ctx.read_file_state,
                &canon,
                current_mtime_ms,
                cmp_content,
                crate::read_requirement_waived(Some(&ctx.options.main_loop_model), &canon),
            ) {
                self.emit_failed(&invocation_id, "stale_read").await;
                return Err(e);
            }
        }

        // (/rewind) Back up the pre-write content (or record a null backup for a
        // brand-new file) BEFORE writing so /rewind can restore it.
        if let Some(fh) = ctx.file_history.as_ref() {
            fh.track_edit(&canon.to_string_lossy()).await;
        }

        let Some((root, relative)) = crate::shared::rooted_location(&canon, &trusted_dirs) else {
            self.emit_failed(&invocation_id, "path_blocked").await;
            return Err(ToolError::PathBlocked { path });
        };
        let write_result = match lingxi_core::host::rooted_fs::write_file_after_permission(
            &root,
            &relative,
            &path,
            &canon,
            content.as_bytes(),
        ) {
            Ok(result) => result,
            Err(error) => {
                self.emit_failed(&invocation_id, "io_write").await;
                return Err(write_resolution_error(file_path, error));
            }
        };

        let structured_patch = if is_create {
            Vec::new()
        } else {
            crate::structured_patch::build_structured_patch(
                prior_decoded.as_deref().unwrap_or(""),
                content,
            )
        };
        let (lines_added, lines_removed) = count_patch_lines(&structured_patch);
        if lines_added > 0 || lines_removed > 0 {
            // The oracle records the MODEL that made the change, not the tool.
            otel::record_lines_of_code_change(
                &ctx.options.main_loop_model,
                lines_added,
                lines_removed,
            );
        }

        let bytes_written = content.as_bytes().len() as u64;
        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, bytes_written, duration_ms)
            .await;

        // Post-write: update the read-state registry so an immediate second
        // Write/Edit succeeds and Read-dedup sees the new mtime (TS
        // `FileWriteTool.ts:332-337` `readFileState.set({content, timestamp:
        // <new mtime>, offset: undefined, limit: undefined})`). Write stores
        // the model-sent `content` verbatim (it is written as UTF-8/LF), with
        // offset/limit cleared so the next read counts as a full read.
        let new_mtime_ms = write_result
            .modified
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        tool_api::read_file_state::set(
            &self.ctx.read_file_state,
            canon.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: content.to_string(),
                mtime_ms: new_mtime_ms,
                offset: None,
                limit: None,
                // A post-write entry is NOT a Read — the Read-dedup gate must
                // skip it (TS stores `offset: undefined` here; we flag it).
                from_read: false,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );

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
        let structured_patch = if is_create {
            serde_json::json!([])
        } else {
            serde_json::json!(structured_patch)
        };

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
            Value::Null
        } else {
            Value::String(prior_decoded.clone().unwrap_or_default())
        };

        Ok(ToolCallResult {
            data: json!({
                "type": type_str,
                "filePath": file_path,
                "content": content,
                "structuredPatch": structured_patch,
                "originalFile": original_file,
                "userModified": false,
            }),
            // Model-facing text is the byte-faithful Write message, surfaced
            // verbatim (NOT a JSON dump of `data`).
            model_content: Some(content_message),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    /// the canonicalized key with `offset`/`limit` = `None`. Call AFTER writing
    /// the file so the seeded mtime matches the on-disk mtime.
    fn seed_full_read(ctx: &BuiltinToolContext, target: &std::path::Path) {
        let canon = std::fs::canonicalize(target).unwrap();
        let bytes = std::fs::read(&canon).unwrap();
        let content = crate::shared::decode_utf8_strict(&bytes)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned());
        let mtime_ms = std::fs::metadata(&canon)
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        tool_api::read_file_state::set(
            &ctx.read_file_state,
            canon,
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
        seed_full_read(&ctx, &target);
        let tool = FileWriteTool::new(ctx);

        let error = tool
            .call(
                json!({ "file_path": link.to_string_lossy(), "content": "changed" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("Write must refuse a final symlink");
        let message = match error {
            tool_api::tool_trait::ToolError::InvalidInput(message) => message,
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(message.starts_with("Refusing to write "));
        assert!(
            message.ends_with("it is a symbolic link. Write to the link's target path instead.")
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn write_resolution_messages_are_byte_exact() {
        assert_eq!(
            write_resolution_error("/tmp/link.txt", lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink)
                .to_string()
                .strip_prefix("invalid input: ")
                .unwrap_or_default(),
            "Refusing to write /tmp/link.txt: it is a symbolic link. Write to the link's target path instead."
        );
        assert_eq!(
            write_resolution_error(
                "/tmp/link.txt",
                lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged,
            )
            .to_string(),
            "invalid input: Refusing to write /tmp/link.txt: its parent-directory symlink resolution changed after permission was checked."
        );
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
            serde_json::json!({ "file_path": "/workspace/abc/new.txt", "content": "from guest" }),
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
        // model:None ⇒ Dh(undefined)=false ⇒ LONG (byte-anchor).
        let long = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: None,
                model_profile: None,
            })
            .await;
        assert_eq!(long, WRITE_PROMPT_LONG);
        assert!(long.starts_with("Writes a file to the local filesystem.\n\nUsage:"));
        assert!(long.contains("you MUST use the Read tool first"));
        // model:claude-opus-4-8 ⇒ Dh=true ⇒ SHORT (byte-anchor).
        let short = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: Some("claude-opus-4-8".to_string()),
                model_profile: None,
            })
            .await;
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
                json!({ "file_path": target.to_str().unwrap(), "content": "hello" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "create");
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
                json!({ "file_path": target.to_str().unwrap(), "content": "hello" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // New file → `create`. `data.content` is the FILE BYTES written (NOT
        // the message); the model message is surfaced verbatim via
        // `model_content` and echoes the ORIGINAL input path.
        let input_path = target.to_str().unwrap();
        assert_eq!(result.data["type"], "create");
        assert_eq!(result.data["filePath"], input_path);
        assert_eq!(result.data["content"], "hello");
        assert_eq!(result.data["originalFile"], Value::Null);
        assert_eq!(result.data["userModified"], false);
        assert!(result.data.get("bytes_written").is_none());
        assert!(result.data.get("gitDiff").is_none());
        assert_eq!(
            result.model_content.as_deref().unwrap(),
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
                json!({ "file_path": target.to_str().unwrap(), "content": "findings" }),
                subagent_ctx,
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => assert_eq!(
                m,
                "Subagents should return findings as text, not write report files. Include this content in your final response instead."
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
                json!({ "file_path": target.to_str().unwrap(), "content": "ok" }),
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
                json!({ "file_path": target.to_str().unwrap(), "content": "hello\nworld\n" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "create");
        // create → `structuredPatch: []` (empty hunk array, TS parity).
        assert_eq!(result.data["structuredPatch"], json!([]));
        // `create` → originalFile is null (TS "null for new files").
        assert_eq!(result.data["originalFile"], Value::Null);
        // `data.content` is the FILE BYTES written, not the message.
        assert_eq!(result.data["content"], "hello\nworld\n");
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
                json!({ "file_path": target.to_str().unwrap(), "content": "alpha\nBETA\ngamma\n" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "update");
        // `originalFile` is the pre-write content; `content` is the new bytes.
        assert_eq!(result.data["originalFile"], "alpha\nbeta\ngamma\n");
        assert_eq!(result.data["content"], "alpha\nBETA\ngamma\n");
        // The structuredPatch is the jsdiff hunk array capturing the changed
        // middle line (1:1: context + `-beta`/`+BETA` + context).
        let sp = result.data["structuredPatch"]
            .as_array()
            .expect("hunk array");
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
                json!({ "file_path": target.to_str().unwrap(), "content": "hi" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "create");
        assert_eq!(result.data["content"], "hi");
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
                json!({
                    "file_path": target.to_str().unwrap(),
                    "content": "deep content"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["content"], "deep content");
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
                json!({ "file_path": target.to_str().unwrap(), "content": "nested" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "create");
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
                json!({ "file_path": target.to_str().unwrap(), "content": "new" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Pre-existing non-empty file → `update`: byte-faithful message lives on
        // `model_content`; `data.content` is the new FILE BYTES.
        let input_path = target.to_str().unwrap();
        assert_eq!(result.data["type"], "update");
        assert_eq!(result.data["filePath"], input_path);
        assert_eq!(result.data["content"], "new");
        assert_eq!(result.data["originalFile"], "old");
        assert_eq!(result.data["userModified"], false);
        assert_eq!(
            result.model_content.as_deref().unwrap(),
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
                json!({ "file_path": target.to_str().unwrap(), "content": "filled" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let input_path = target.to_str().unwrap();
        assert_eq!(result.data["type"], "create");
        // create → originalFile is null even for an existing-but-empty file
        // (TS `if (oldContent)` is falsy → create branch, `originalFile:null`).
        assert_eq!(result.data["originalFile"], Value::Null);
        assert_eq!(result.data["content"], "filled");
        assert_eq!(
            result.model_content.as_deref().unwrap(),
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
                json!({
                    "file_path": target.to_str().unwrap(),
                    "content": "deep content"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let input_path = target.to_str().unwrap();
        assert_eq!(result.data["type"], "create");
        assert_eq!(result.data["content"], "deep content");
        assert_eq!(
            result.model_content.as_deref().unwrap(),
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
                json!({ "file_path": target.to_str().unwrap(), "content": "new" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => assert_eq!(m, crate::FILE_NOT_READ_ERROR),
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
                json!({ "file_path": target.to_str().unwrap(), "content": "brand new" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "create");
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
                json!({ "file_path": target.to_str().unwrap(), "content": "model wrote this" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            // FT-07: stale-changed-content returns the oracle's
            // `validateInput` errorCode-3 literal (cc-238.js @226416137),
            // not the call-phase race sentence `WVo`.
            ToolError::InvalidInput(m) => assert_eq!(m, crate::FILE_UNEXPECTEDLY_MODIFIED_ERROR),
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
                json!({ "file_path": target.to_str().unwrap(), "content": "overwritten" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Pre-existing non-empty file → update.
        assert_eq!(result.data["type"], "update");
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
            json!({ "file_path": target.to_str().unwrap(), "content": "v1" }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // No re-seed: the post-write `set` from the first write must satisfy
        // the guard for the second write.
        tool.call(
            json!({ "file_path": target.to_str().unwrap(), "content": "v2" }),
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
                json!({ "file_path": target.to_str().unwrap(), "content": "x" }),
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
        assert!(tool.is_destructive(&json!({})));
    }

    #[tokio::test]
    async fn is_concurrency_safe_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        assert!(!tool.is_concurrency_safe(&json!({})));
    }

    #[tokio::test]
    async fn is_read_only_returns_false() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        assert!(!tool.is_read_only(&json!({})));
    }

    #[tokio::test]
    async fn input_schema_requires_file_path_and_content() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileWriteTool::new(ctx);
        let schema = tool.input_schema();
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"file_path"));
        assert!(names.contains(&"content"));
        // claude-code schema is {file_path, content} ONLY — no `mkdir`.
        assert!(
            schema["properties"].get("mkdir").is_none(),
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
                json!({ "file_path": "relative.txt", "content": "hello" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "create");
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
                json!({ "file_path": "relative.txt", "content": "hello" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["type"], "create");
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
        async fn submit(&self, _: Value) -> Result<Value, String> {
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
            json!({"file_path":target,"content":"approved scope"}),
            context(),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved scope");
        assert!(tool
            .call(
                json!({"file_path":sibling,"content":"not allowed"}),
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
                    json!({"file_path":target,"content":"no"}),
                    context(),
                    fresh_tx()
                )
                .await
                .is_err());
            assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "protected");
        }
    }
}
