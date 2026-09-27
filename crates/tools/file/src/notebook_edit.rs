//! `NotebookEditTool` — replace / insert / delete a cell in a Jupyter `.ipynb`.
//!
//! `.ipynb` is JSON with `cells: [{ id, source, ... }, ...]`. We round-trip
//! through `serde_json::Value` so `preserve_order` (workspace feature) keeps
//! key ordering byte-stable.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{NOTEBOOK_COMPLETED, NOTEBOOK_FAILED, NOTEBOOK_STARTED};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, translate_model_path,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "NotebookEdit";

/// Allowed `edit_mode` value: replace a cell's source.
pub const EDIT_MODE_REPLACE: &str = "replace";
/// Allowed `edit_mode` value: insert a new cell after the named anchor.
pub const EDIT_MODE_INSERT: &str = "insert";
/// Allowed `edit_mode` value: delete the named cell.
pub const EDIT_MODE_DELETE: &str = "delete";

/// `NotebookEditTool` — mutate one cell of a Jupyter notebook.
pub struct NotebookEditTool {
    ctx: BuiltinToolContext,
}

impl NotebookEditTool {
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
            "_PROTO_notebook_path".to_string(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(path.display().to_string()).into_inner(),
            ),
        );
        self.ctx.bus.log_event(NOTEBOOK_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, cells_edited: u32, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "cells_edited".to_string(),
            AnalyticsValue::Int(cells_edited as i64),
        );
        md.insert(
            "duration_ms".to_string(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(NOTEBOOK_COMPLETED, md).await;
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
        self.ctx.bus.log_event(NOTEBOOK_FAILED, md).await;
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    // Requiredness mirrors the TS `inputSchema` strictObject
    // (`NotebookEditTool.ts:30-57`): only `notebook_path` + `new_source` are
    // required; `cell_id`, `cell_type`, and `edit_mode` are optional.
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["notebook_path", "new_source"],
        "properties": {
            "notebook_path": { "type": "string", "description": "The absolute path to the Jupyter notebook file to edit (must be absolute, not relative)" },
            "cell_id":       { "type": "string", "description": "The ID of the cell to edit. When inserting a new cell, the new cell will be inserted after the cell with this ID, or at the beginning if not specified." },
            "new_source":    { "type": "string", "description": "The new source for the cell" },
            "cell_type":     {
                "type": "string",
                "enum": ["code", "markdown"],
                "description": "The type of the cell (code or markdown). If not specified, it defaults to the current cell type. If using edit_mode=insert, this is required."
            },
            "edit_mode":     {
                "type": "string",
                "enum": ["replace", "insert", "delete"],
                "description": "The type of edit to make (replace, insert, delete). Defaults to replace."
            }
        }
    })
});

/// Parse a `cell-N` id into its numeric index, mirroring the TS `parseCellId`
/// helper (`utils/notebook.ts`): matches `^cell-(\d+)$` and returns N, else
/// `None`. Used as the fallback when an exact cell-`id` lookup misses.
fn parse_cell_id(cell_id: &str) -> Option<usize> {
    let digits = cell_id.strip_prefix("cell-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok()
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<&str> {
        Some("edit Jupyter notebook cells (.ipynb)")
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
        // Verbatim claude-code v2.1.183 NotebookEdit description const `f2a`
        // (em-dash is U+2014).
        "Edit a cell in a Jupyter notebook — replace, insert, or delete.".to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        // Verbatim claude-code v2.1.183 NotebookEdit PROMPT const `A2a` (the
        // older cell_number-based text was version-skewed; v2.1.183 is cell_id-
        // based). `${Ws}` interpolates to the Read tool name (`Ws="Read"`, binary
        // offset 195604698) — NOT "NotebookRead"; em-dashes are U+2014; the
        // backticks are literal markdown code spans. Note the BLANK LINE (`\n\n`)
        // between the opening sentence and `Usage:` (binary A2a, offset 201376292).
        "Replaces, inserts, or deletes a single cell in a Jupyter notebook (.ipynb file).\n\
\n\
Usage:\n\
- You must use the Read tool on the notebook in this conversation before editing — this tool will fail otherwise.\n\
- `notebook_path` must be an absolute path.\n\
- `cell_id` is the `id` attribute shown in the Read tool's `<cell id=\"...\">` output. It is required for `replace` and `delete`.\n\
- `edit_mode` defaults to `replace`. Use `insert` to add a new cell after the cell with the given `cell_id` (or at the beginning of the notebook if `cell_id` is omitted) — `cell_type` is required when inserting. Use `delete` to remove the cell.".to_string()
    }

    fn get_path(&self, input: &Value) -> Option<PathBuf> {
        input
            .get("notebook_path")
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
        let notebook_path = input
            .get("notebook_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("notebook_path is required".into()))?;
        // `cell_id` and `edit_mode` are optional per the TS schema
        // (`NotebookEditTool.ts:37-55`): `cell_id` absent means "insert at the
        // beginning"; `edit_mode` defaults to `replace`. `cell_type` is the
        // newly-honored insert/replace cell kind.
        let cell_id = input.get("cell_id").and_then(Value::as_str);
        let edit_mode = input
            .get("edit_mode")
            .and_then(Value::as_str)
            .unwrap_or(EDIT_MODE_REPLACE);
        let mut cell_type = input
            .get("cell_type")
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string);
        let new_source = input
            .get("new_source")
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string);

        let path = PathBuf::from(notebook_path);
        let started = Instant::now();
        self.emit_started(&invocation_id, &path).await;

        // Up-front validation, mirroring TS `validateInput`
        // (`NotebookEditTool.ts:198-216`): the edit_mode enum and the
        // cell_type-required-for-insert rule. Both run before any file I/O.
        if edit_mode != EDIT_MODE_REPLACE
            && edit_mode != EDIT_MODE_INSERT
            && edit_mode != EDIT_MODE_DELETE
        {
            self.emit_failed(&invocation_id, "bad_edit_mode").await;
            // [RESIDUAL — message not in 2.1.206] claude rejects an invalid
            // `edit_mode` at the SCHEMA layer (the Zod enum on the input), so
            // its wording is the validator's own non-reproducible output; this
            // in-tool fallback string is port-only, reachable only when the
            // model bypasses the schema enum.
            return Err(ToolError::InvalidInput(
                "Edit mode must be replace, insert, or delete.".into(),
            ));
        }
        if edit_mode == EDIT_MODE_INSERT && cell_type.is_none() {
            self.emit_failed(&invocation_id, "cell_type_required").await;
            return Err(ToolError::InvalidInput(
                "Cell type is required when using edit_mode=insert.".into(),
            ));
        }
        // The path must be a Jupyter notebook (`NotebookEditTool.ts:189-196`,
        // `extname(fullPath) !== '.ipynb'`). Case-insensitive on the extension.
        if path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("ipynb"))
        {
            self.emit_failed(&invocation_id, "not_a_notebook").await;
            return Err(ToolError::InvalidInput(
                "File must be a Jupyter notebook (.ipynb file). For editing other file types, use the FileEdit tool.".into(),
            ));
        }

        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, path, true) {
            Ok(path) => path,
            Err(message) => {
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::InvalidInput(message));
            }
        };
        let canon = match canonicalize_and_validate(&path, &self.ctx.trusted_dirs()) {
            Ok(p) => p,
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::PathBlocked { path });
            }
        };

        let raw = match tokio::fs::read_to_string(&canon).await {
            Ok(s) => s,
            Err(e) => {
                self.emit_failed(&invocation_id, "io_read").await;
                return Err(ToolError::Io(e.to_string()));
            }
        };

        // Read-before-write staleness guard (Batch F). The notebook always
        // exists here (the `read_to_string` above errors otherwise), so a prior
        // full read + unchanged-mtime is required (TS
        // `NotebookEditTool.ts:221-237`). We reuse the shared
        // `check_read_before_write` helper for uniformity with Edit/Write,
        // which also applies the isPartialView + content-equality fallback —
        // TS's NotebookEdit guard is the simpler `!lastRead` / `mtime >
        // timestamp` form without those.
        //
        // The Rust `Read` tool DOES read `.ipynb` (read.rs ipynb branch) and
        // records the read into `read_file_state` — with the serialized CELLS
        // JSON as the entry `content` (mirroring TS `FileReadTool.ts:842-847`).
        // So a plain Read→NotebookEdit satisfies this guard via the mtime
        // check. The content-equality fallback compares the RAW on-disk bytes
        // (`raw`) against that recorded cells JSON and thus never matches for a
        // notebook — which collapses the shared helper to exactly TS's
        // no-fallback `mtime > timestamp` NotebookEdit guard.
        let current_mtime_ms = tokio::fs::metadata(&canon)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        if let Err(e) = crate::check_read_before_write(
            &self.ctx.read_file_state,
            &canon,
            current_mtime_ms,
            &raw,
            // Oracle `X_n(S)`: notebooks are excluded from the waiver
            // outright — their read guard is cell-structured, so "the model
            // could have read it" does not establish that it saw the cell.
            false,
        ) {
            self.emit_failed(&invocation_id, "stale_read").await;
            return Err(e);
        }

        let mut nb: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => {
                // SOFT return (not an error): claude-code returns a result whose
                // `data` carries `error: 'Notebook is not valid JSON.'` rather
                // than throwing (`NotebookEditTool.ts:331-348`). Mirror that
                // data shape so the model sees the soft error.
                self.emit_failed(&invocation_id, "json_parse").await;
                return Ok(ToolCallResult {
                    data: json!({
                        "new_source": new_source,
                        "cell_type": cell_type.clone().unwrap_or_else(|| "code".to_string()),
                        "language": "python",
                        "edit_mode": "replace",
                        "error": "Notebook is not valid JSON.",
                        "cell_id": cell_id,
                        "notebook_path": canon.display().to_string(),
                        "original_file": "",
                        "updated_file": "",
                    }),
                    // The soft error rides on `model_content` (the data carries no
                    // content/model_content key, so the fallback would JSON-dump).
                    model_content: Some("Notebook is not valid JSON.".to_string()),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                });
            }
        };

        // Only nbformat >= 4.5 notebooks carry stable cell ids
        // (`NotebookEditTool.ts:381-384`). Read this before borrowing `cells`
        // mutably below.
        let supports_cell_ids = {
            let nbformat = nb.get("nbformat").and_then(Value::as_i64).unwrap_or(0);
            let nbformat_minor = nb
                .get("nbformat_minor")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            nbformat > 4 || (nbformat == 4 && nbformat_minor >= 5)
        };

        let cells = nb
            .get_mut("cells")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| {
                // Binary `readNotebook` (atl): byte-locked invalid-cells error.
                ToolError::InvalidInput(
                    "Notebook file is not a valid Jupyter notebook (top-level \"cells\" must be an array of cell objects, each with a string or string-array \"source\").".into(),
                )
            })?;
        let cells_len = cells.len();

        // A missing `cell_id` is only valid for `insert` (TS validateInput
        // `NotebookEditTool.ts:260-267`, errorCode 7).
        if cell_id.is_none() && edit_mode != EDIT_MODE_INSERT {
            self.emit_failed(&invocation_id, "cell_id_required").await;
            return Err(ToolError::InvalidInput(
                "Cell ID must be specified when not inserting a new cell.".into(),
            ));
        }

        // Resolve the target index, mirroring TS `call` (`NotebookEditTool.ts:
        // 350-368`): exact `id` match first, then the `cell-N` index form
        // (`parseCellId`). The not-found / out-of-bounds rejections are folded
        // in from TS `validateInput` (lines 268-290) since the Rust tool has no
        // separate validate phase.
        let mut cell_index: usize = match cell_id {
            // No cell_id → default to inserting at the beginning (ts:351-352).
            None => 0,
            Some(id) => {
                if let Some(i) = cells
                    .iter()
                    .position(|c| c.get("id").and_then(Value::as_str) == Some(id))
                {
                    i
                } else if let Some(n) = parse_cell_id(id) {
                    if n >= cells_len {
                        self.emit_failed(&invocation_id, "cell_not_found").await;
                        return Err(ToolError::InvalidInput(format!(
                            "Cell with index {n} does not exist in notebook."
                        )));
                    }
                    n
                } else {
                    self.emit_failed(&invocation_id, "cell_not_found").await;
                    return Err(ToolError::InvalidInput(format!(
                        "Cell with ID \"{id}\" not found in notebook."
                    )));
                }
            }
        };

        // Insert lands AFTER the anchor when a cell_id was supplied (ts:365-367).
        if edit_mode == EDIT_MODE_INSERT && cell_id.is_some() {
            cell_index += 1;
        }

        // A `replace` that targets one past the end becomes an `insert`,
        // defaulting cell_type to code (ts:370-377).
        let mut effective_mode = edit_mode;
        if effective_mode == EDIT_MODE_REPLACE && cell_index == cells_len {
            effective_mode = EDIT_MODE_INSERT;
            if cell_type.is_none() {
                cell_type = Some("code".to_string());
            }
        }

        // New-cell id (ts:380-390): inserts mint a FRESH id (never the anchor's,
        // which previously produced duplicates); replace/delete reuse the
        // supplied cell_id. Both only when the notebook supports cell ids.
        let new_cell_id: Option<String> = if supports_cell_ids {
            if effective_mode == EDIT_MODE_INSERT {
                Some(tool_api::util::ids::ulid_or_uuid())
            } else {
                cell_id.map(std::string::ToString::to_string)
            }
        } else {
            None
        };
        // The model-facing message renders this id; an absent id prints
        // "undefined" exactly as the TS template literal would (ts:148-162).
        let display_id = new_cell_id.as_deref().unwrap_or("undefined");

        // Model-facing result string per edit mode — byte-faithful to
        // claude-code's `NotebookEditTool` mapper (`NotebookEditTool.ts:145-170`).
        // FILE.A's serialization rule emits `data["content"]` verbatim to the
        // model (`cells_edited` below remains the structured TUI payload).
        // The result record echoes the new source even on delete (binary `t`).
        let new_source_result = new_source.clone().unwrap_or_default();
        // Capture the cell's prior source for the result `old_source` (binary
        // NotebookEdit field); stays `None` on insert (no prior cell).
        let mut old_source: Option<String> = None;
        let content = match effective_mode {
            EDIT_MODE_DELETE => {
                // `cell_id` was required + resolved above, so the index is valid.
                old_source = cells[cell_index]
                    .get("source")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                cells.remove(cell_index);
                format!("Deleted cell {display_id}")
            }
            EDIT_MODE_INSERT => {
                let src = new_source.ok_or_else(|| {
                    ToolError::InvalidInput("new_source required for insert".into())
                })?;
                // Build the message before `src` is moved into the new cell.
                let msg = format!("Inserted cell {display_id} with {src}");
                let is_markdown = cell_type.as_deref() == Some("markdown");
                // Key order matches the TS object literals (ts:396-413).
                let mut new_cell = serde_json::Map::new();
                new_cell.insert(
                    "cell_type".to_string(),
                    json!(if is_markdown { "markdown" } else { "code" }),
                );
                // `id: undefined` is dropped by `JSON.stringify`, so only emit
                // the key when the notebook supports cell ids.
                if let Some(id) = &new_cell_id {
                    new_cell.insert("id".to_string(), json!(id));
                }
                new_cell.insert("source".to_string(), json!(src));
                new_cell.insert("metadata".to_string(), json!({}));
                if !is_markdown {
                    new_cell.insert("execution_count".to_string(), Value::Null);
                    new_cell.insert("outputs".to_string(), json!([]));
                }
                cells.insert(cell_index, Value::Object(new_cell));
                msg
            }
            EDIT_MODE_REPLACE => {
                let src = new_source.ok_or_else(|| {
                    ToolError::InvalidInput("new_source required for replace".into())
                })?;
                let msg = format!("Updated cell {display_id} with {src}");
                let target = &mut cells[cell_index];
                old_source = target
                    .get("source")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let was_code = target.get("cell_type").and_then(Value::as_str) == Some("code");
                target["source"] = json!(src);
                // A modified CODE cell drops its now-stale outputs +
                // execution_count (ts:420-424).
                if was_code {
                    target["execution_count"] = Value::Null;
                    target["outputs"] = json!([]);
                }
                // An explicit cell_type that differs switches the cell's type
                // (ts:425-427).
                if let Some(ct) = &cell_type {
                    if target.get("cell_type").and_then(Value::as_str) != Some(ct.as_str()) {
                        target["cell_type"] = json!(ct);
                    }
                }
                msg
            }
            // `edit_mode` was validated to the three modes above, and the
            // replace→insert conversion only yields `insert`.
            _ => unreachable!("edit_mode validated above"),
        };

        // claude-code writes the notebook with `IPYNB_INDENT = 1` (ONE space) —
        // `jsonStringify(notebook, null, 1)` (NotebookEditTool.ts:430-431).
        // serde_json's `to_string_pretty` uses TWO spaces, which would write
        // divergent on-disk bytes (different file hash / noisier git diffs for a
        // round-tripped notebook). Serialize with a 1-space pretty formatter.
        let serialized = {
            use serde::Serialize as _;
            let mut buf = Vec::new();
            let mut ser = serde_json::Serializer::with_formatter(
                &mut buf,
                serde_json::ser::PrettyFormatter::with_indent(b" "),
            );
            match nb.serialize(&mut ser) {
                Ok(()) => String::from_utf8(buf).expect("serde_json emits valid UTF-8"),
                Err(e) => {
                    self.emit_failed(&invocation_id, "json_emit").await;
                    return Err(ToolError::Io(e.to_string()));
                }
            }
        };
        // (/rewind) Back up the pre-edit notebook before writing.
        if let Some(fh) = ctx.file_history.as_ref() {
            fh.track_edit(&canon.to_string_lossy()).await;
        }
        if let Err(e) = tokio::fs::write(&canon, serialized.as_bytes()).await {
            self.emit_failed(&invocation_id, "io_write").await;
            return Err(ToolError::Io(e.to_string()));
        }

        // Post-write: update the read-state registry so an immediate second
        // NotebookEdit succeeds and the staleness guard sees the new mtime (TS
        // `NotebookEditTool.ts:437-442` `readFileState.set({content:
        // updatedContent, timestamp: <new mtime>, offset: undefined, limit:
        // undefined})`). `serialized` is the just-written notebook JSON;
        // offset/limit cleared so it counts as a full read.
        let new_mtime_ms = tokio::fs::metadata(&canon)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        tool_api::read_file_state::set(
            &self.ctx.read_file_state,
            canon.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: serialized.clone(),
                mtime_ms: new_mtime_ms,
                offset: None,
                limit: None,
                // Post-edit entry — not a Read.
                from_read: false,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );

        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, 1, duration_ms).await;

        // claude-code NotebookEditTool result data (2.1.191): the full edit record
        // `{new_source, old_source?, cell_type, language, edit_mode, cell_id?,
        // error, notebook_path, original_file, updated_file}` (preserve_order).
        // `old_source`/`cell_id` are omitted when absent (binary `void 0`); `error`
        // is "" on success. The model-facing message rides on `model_content`.
        let mut nb_data = serde_json::Map::new();
        nb_data.insert("new_source".to_string(), json!(new_source_result));
        if let Some(os) = &old_source {
            nb_data.insert("old_source".to_string(), json!(os));
        }
        nb_data.insert(
            "cell_type".to_string(),
            json!(cell_type.as_deref().unwrap_or("code")),
        );
        nb_data.insert("language".to_string(), json!("python"));
        // Binary success data: `edit_mode:T??"replace"` where T is the EFFECTIVE
        // mode — a replace targeting one-past-the-end is reported as "insert"
        // (the conversion at the cell_index==cells_len check above).
        nb_data.insert("edit_mode".to_string(), json!(effective_mode));
        if let Some(cid) = cell_id {
            nb_data.insert("cell_id".to_string(), json!(cid));
        }
        nb_data.insert("error".to_string(), json!(""));
        nb_data.insert(
            "notebook_path".to_string(),
            json!(canon.display().to_string()),
        );
        nb_data.insert("original_file".to_string(), json!(raw));
        nb_data.insert("updated_file".to_string(), json!(serialized));
        Ok(ToolCallResult {
            data: Value::Object(nb_data),
            model_content: Some(content),
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

    pub(crate) fn make_ctx(tmp: &TempDir) -> (BuiltinToolContext, Arc<InMemorySink>) {
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

    /// Simulate a prior full read of `target` (a notebook) so the
    /// read-before-write staleness guard (Batch F) is satisfied. Records the
    /// file's current content + floored mtime under the canonicalized key with
    /// `offset`/`limit` = `None`. Call AFTER writing the file's bytes.
    ///
    /// NB: the Rust `Read` tool cannot populate this for `.ipynb` (no notebook
    /// media support), so in production only a prior NotebookEdit's own
    /// post-write `set` satisfies the guard — these tests seed it directly to
    /// exercise the post-guard logic. (Documented partial-coverage gap.)
    fn seed_full_read(ctx: &BuiltinToolContext, target: &std::path::Path) {
        let canon = std::fs::canonicalize(target).unwrap();
        let content = std::fs::read_to_string(&canon).unwrap();
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

    pub(crate) fn sample_notebook() -> String {
        serde_json::to_string_pretty(&json!({
            "cells": [
                { "cell_type": "code", "id": "c1", "source": "print('hello')", "metadata": {}, "outputs": [], "execution_count": null },
                { "cell_type": "markdown", "id": "c2", "source": "# Heading", "metadata": {} }
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        })).unwrap()
    }

    #[test]
    fn tool_name_is_notebook_edit() {
        assert_eq!(TOOL_NAME, "NotebookEdit");
    }

    /// A REAL `Read` of the notebook satisfies the read-before-write guard —
    /// no manual seeding. Read's ipynb branch records the read (cells JSON +
    /// mtime) into the SHARED `read_file_state`, so the subsequent NotebookEdit
    /// passes the guard via the mtime check exactly like TS's
    /// Read→NotebookEdit flow. (Guards the staleness-guard wiring against a
    /// regression where the ipynb Read stops recording state, which would make
    /// every Read→NotebookEdit fail with "File has not been read yet".)
    #[tokio::test]
    async fn real_read_then_notebook_edit_passes_the_guard() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // Run the actual Read tool (ipynb branch) on a CLONE of the ctx — the
        // read_file_state map is an Arc, so the recording is shared.
        let read_tool = crate::read::FileReadTool::new(ctx.clone());
        read_tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Read must handle .ipynb");
        // No seed_full_read: the Read above must be sufficient.
        let tool = NotebookEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "print('via-real-read')"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Read→NotebookEdit must satisfy the staleness guard");
        assert_eq!(
            result.model_content.as_deref(),
            Some("Updated cell c1 with print('via-real-read')")
        );
    }

    #[tokio::test]
    async fn replace_mode_swaps_source() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "print('world')"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Model-facing string rides on model_content (the binary result `data` is
        // the {new_source, old_source, …} record — no `content` field).
        assert_eq!(
            result.model_content.as_deref(),
            Some("Updated cell c1 with print('world')")
        );
        assert_eq!(result.data["new_source"], "print('world')");
        assert_eq!(result.data["old_source"], "print('hello')");
        assert_eq!(result.data["edit_mode"], "replace");
        assert_eq!(result.data["error"], "");
        assert!(result.data.get("content").is_none());
        assert!(result.data["updated_file"].is_string());
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(modified["cells"][0]["source"], "print('world')");
    }

    #[tokio::test]
    async fn written_notebook_uses_one_space_indent() {
        // claude-code writes the .ipynb with `IPYNB_INDENT = 1`
        // (NotebookEditTool.ts:430-431). Lock the ON-DISK byte shape: top-level
        // keys are indented by exactly ONE space — NOT serde_json's two-space
        // `to_string_pretty` default (which would diverge file bytes/hashes).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "replace",
                "new_source": "print('world')"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let written = std::fs::read_to_string(&target).unwrap();
        assert!(
            written.contains("\n \"cells\":") || written.contains("\n \"nbformat\":"),
            "notebook must use 1-space indent (claude-code IPYNB_INDENT=1); got head:\n{}",
            &written[..written.len().min(160)]
        );
        assert!(
            !written.contains("\n  \"cells\":") && !written.contains("\n  \"nbformat\":"),
            "notebook must NOT use serde_json's 2-space indent"
        );
    }

    #[tokio::test]
    async fn delete_mode_removes_cell() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "delete"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Model-facing string rides on model_content; data is the edit record.
        assert_eq!(result.model_content.as_deref(), Some("Deleted cell c1"));
        assert_eq!(result.data["edit_mode"], "delete");
        assert_eq!(result.data["old_source"], "print('hello')");
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let cells = modified["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0]["id"], "c2");
    }

    #[tokio::test]
    async fn insert_mode_adds_cell_after_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "insert",
                    "cell_type": "code",
                    "new_source": "print('inserted')"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // The message renders the FRESH cell id (NB.2 / NotebookEditTool.ts:
        // 152-156 with cell_id = new_cell_id), so only the static framing is
        // byte-fixed.
        let content = result.model_content.as_deref().unwrap();
        assert!(content.starts_with("Inserted cell "));
        assert!(content.ends_with(" with print('inserted')"));
        // Insert has no prior cell → old_source omitted (binary `void 0`).
        assert!(result.data.get("old_source").is_none());
        assert_eq!(result.data["edit_mode"], "insert");
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let cells = modified["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1]["source"], "print('inserted')");
        // NB.2: the inserted cell gets a fresh id, never the anchor's "c1".
        let new_id = cells[1]["id"].as_str().unwrap();
        assert!(!new_id.is_empty());
        assert_ne!(new_id, "c1");
        // The fresh id also appears in the model-facing message.
        assert_eq!(
            content,
            format!("Inserted cell {new_id} with print('inserted')")
        );
    }

    // ───────────────────────── NB.1: replace clears code outputs ────────────

    fn code_notebook_with_outputs() -> String {
        serde_json::to_string_pretty(&json!({
            "cells": [
                {
                    "cell_type": "code",
                    "id": "c1",
                    "source": "print('hello')",
                    "metadata": {},
                    "outputs": [ { "output_type": "stream", "name": "stdout", "text": "hello\n" } ],
                    "execution_count": 7
                },
                { "cell_type": "markdown", "id": "c2", "source": "# Heading", "metadata": {} }
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn replace_clears_outputs_and_execution_count_for_code_cell() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, code_notebook_with_outputs()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "replace",
                "new_source": "print('world')"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let cell = &modified["cells"][0];
        assert_eq!(cell["source"], "print('world')");
        // NB.1: a modified code cell loses its stale outputs + execution_count.
        assert_eq!(cell["outputs"], json!([]));
        assert!(cell["execution_count"].is_null());
    }

    #[tokio::test]
    async fn replace_leaves_markdown_outputs_untouched() {
        // A markdown cell has no outputs/execution_count; NB.1 must not add any.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, code_notebook_with_outputs()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c2",
                "edit_mode": "replace",
                "new_source": "## Updated"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let cell = &modified["cells"][1];
        assert_eq!(cell["source"], "## Updated");
        assert!(cell.get("outputs").is_none());
        assert!(cell.get("execution_count").is_none());
    }

    // ───────────────────────── NB.2: fresh, unique insert ids ───────────────

    #[tokio::test]
    async fn two_inserts_produce_distinct_ids() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        for src in ["print('a')", "print('b')"] {
            tool.call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "insert",
                    "cell_type": "code",
                    "new_source": src
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        }
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let cells = modified["cells"].as_array().unwrap();
        let ids: Vec<&str> = cells
            .iter()
            .filter_map(|c| c.get("id").and_then(Value::as_str))
            .collect();
        // No duplicate ids anywhere in the notebook.
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate cell ids: {ids:?}");
    }

    // ───────────────────────── NB.3: cell-N index resolution ────────────────

    #[tokio::test]
    async fn resolves_cell_n_index_form() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        // "cell-0" addresses the first cell by index (parseCellId).
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "cell-0",
                "edit_mode": "replace",
                "new_source": "print('by index')"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(modified["cells"][0]["source"], "print('by index')");
    }

    #[tokio::test]
    async fn cell_n_out_of_bounds_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "cell-99",
                    "edit_mode": "replace",
                    "new_source": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not exist in notebook"));
    }

    // ───────────────────────── NB.4: cell_type on insert ────────────────────

    #[tokio::test]
    async fn insert_markdown_cell_honors_cell_type() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "insert",
                "cell_type": "markdown",
                "new_source": "# inserted md"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let cell = &modified["cells"][1];
        assert_eq!(cell["cell_type"], "markdown");
        assert_eq!(cell["source"], "# inserted md");
        // markdown cells carry no outputs / execution_count.
        assert!(cell.get("outputs").is_none());
        assert!(cell.get("execution_count").is_none());
    }

    #[tokio::test]
    async fn insert_without_cell_type_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "insert",
                    "new_source": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // Byte-faithful to NotebookEditTool.ts:213.
        assert_eq!(
            err.to_string(),
            "invalid input: Cell type is required when using edit_mode=insert."
        );
    }

    // ───────────────────────── NB.5: schema + insert-at-0 ───────────────────

    #[test]
    fn schema_requiredness_matches_ts() {
        let required = INPUT_SCHEMA["required"].as_array().unwrap();
        assert_eq!(required, &[json!("notebook_path"), json!("new_source")]);
        // cell_type is now a known property (enum code|markdown).
        assert_eq!(
            INPUT_SCHEMA["properties"]["cell_type"]["enum"],
            json!(["code", "markdown"])
        );
    }

    #[tokio::test]
    async fn insert_without_cell_id_lands_at_beginning() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        // No cell_id → insert at position 0 (NotebookEditTool.ts:351-352).
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "edit_mode": "insert",
                "cell_type": "code",
                "new_source": "print('first cell now')"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let cells = modified["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0]["source"], "print('first cell now')");
        assert_eq!(cells[1]["id"], "c1");
    }

    #[tokio::test]
    async fn replace_without_cell_id_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "edit_mode": "replace",
                    "new_source": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // Byte-faithful to NotebookEditTool.ts:264.
        assert!(err
            .to_string()
            .contains("Cell ID must be specified when not inserting a new cell."));
    }

    #[tokio::test]
    async fn rejects_path_outside_trusted() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "delete"
                }),
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
    async fn invalid_json_returns_soft_error_not_err() {
        // claude-code returns a SOFT result with `error: 'Notebook is not valid
        // JSON.'` (NotebookEditTool.ts:331-348), NOT a thrown error.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bad.ipynb");
        std::fs::write(&target, "this is not json").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let result = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "delete"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("malformed JSON is a soft Ok result, not an Err");
        assert_eq!(result.data["error"], "Notebook is not valid JSON.");
        // The soft data shape mirrors TS: edit_mode normalized to "replace",
        // cell_type defaulted to "code", the original cell_id echoed.
        assert_eq!(result.data["edit_mode"], "replace");
        assert_eq!(result.data["cell_type"], "code");
        assert_eq!(result.data["cell_id"], "c1");
        assert_eq!(result.data["language"], "python");
    }

    #[tokio::test]
    async fn rejects_non_ipynb_path() {
        // A non-`.ipynb` path is rejected up front (NotebookEditTool.ts:189-196).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("script.py");
        std::fs::write(&target, "print('hi')").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = NotebookEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => assert_eq!(
                m,
                "File must be a Jupyter notebook (.ipynb file). For editing other file types, use the FileEdit tool."
            ),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    // ───────────────────────── Batch F: staleness guard ─────────────────────

    #[tokio::test]
    async fn notebook_edit_without_prior_read_errors_not_read() {
        // No recorded read of the notebook → FILE_NOT_READ_ERROR. This is the
        // common production path: Rust `Read` cannot populate the registry for
        // `.ipynb` (no notebook media), so a plain edit-without-edit is refused.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // Deliberately do NOT seed a prior read.
        let tool = NotebookEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "print('blocked')"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => assert_eq!(m, crate::FILE_NOT_READ_ERROR),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_write_set_lets_immediate_second_notebook_edit_succeed() {
        // First NotebookEdit succeeds with a seeded read; its post-write `set`
        // updates the registry so a SECOND immediate NotebookEdit (no re-seed)
        // also succeeds — the only way the guard is satisfied in production
        // (Read does not populate the registry for notebooks).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "replace",
                "new_source": "print('first')"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // No re-seed: the post-write set from the first edit satisfies the guard.
        tool.call(
            json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c2",
                "edit_mode": "replace",
                "new_source": "## Updated"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let modified: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(modified["cells"][0]["source"], "print('first')");
        assert_eq!(modified["cells"][1]["source"], "## Updated");
    }

    #[tokio::test]
    async fn rejects_missing_cell_for_replace() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx);
        let err = tool
            .call(
                json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "nonexistent",
                    "edit_mode": "replace",
                    "new_source": "x"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
