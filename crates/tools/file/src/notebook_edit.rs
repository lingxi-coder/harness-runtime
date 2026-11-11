//! `NotebookEditTool` — replace / insert / delete a cell in a Jupyter `.ipynb`.
//!
//! `.ipynb` is JSON with `cells: [{ id, source, ... }, ...]`. We round-trip
//! through `serde_json::Value` so `preserve_order` (workspace feature) keeps
//! key ordering byte-stable.

use crate::Utf16JsonProjection;
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

const MAX_NOTEBOOK_BYTES: usize = 100 * 1024 * 1024;

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

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    // Requiredness mirrors the TS `inputSchema` strictObject
    // (`NotebookEditTool.ts:30-57`): only `notebook_path` + `new_source` are
    // required; `cell_id`, `cell_type`, and `edit_mode` are optional.
    tool_api::tool_trait::generated_builtin_schema(&json!({
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
    }))
});

static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::parse(include_str!(
        "templates/claude-code-2.1.322/notebookedit-output-schema.json"
    ))
    .expect("pinned NotebookEdit output schema is valid JSON")
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

const INVALID_NOTEBOOK: &str = "Notebook file is not a valid Jupyter notebook (top-level \"cells\" must be an array of cell objects, each with a string or string-array \"source\").";

struct NotebookMutation {
    new_source: Utf16JsonProjection,
    old_source: Option<Utf16JsonProjection>,
    language: Utf16JsonProjection,
    cell_id: Option<String>,
    model_content: Utf16JsonProjection,
}

fn notebook_error(message: impl Into<String>) -> ToolError {
    ToolError::InvalidInput(message.into().into())
}

fn joined_cell_source(cell: &Utf16JsonProjection) -> Result<Utf16JsonProjection, ToolError> {
    if cell.string_units("/source").is_some() {
        return cell
            .subprojection("/source")
            .map_err(|_| notebook_error(INVALID_NOTEBOOK));
    }
    let count = cell
        .array_len("/source")
        .ok_or_else(|| notebook_error(INVALID_NOTEBOOK))?;
    let mut units = Vec::new();
    for index in 0..count {
        units.extend(
            cell.string_units(&format!("/source/{index}"))
                .ok_or_else(|| notebook_error(INVALID_NOTEBOOK))?,
        );
    }
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .map_err(|_| notebook_error(INVALID_NOTEBOOK))
}

fn mutate_notebook(
    notebook: &mut Utf16JsonProjection,
    input: &Utf16JsonProjection,
    cell_id: Option<&str>,
    edit_mode: &str,
    cell_type: Option<&str>,
    insert_id: impl FnOnce() -> String,
) -> Result<NotebookMutation, ToolError> {
    let count = notebook
        .array_len("/cells")
        .ok_or_else(|| notebook_error(INVALID_NOTEBOOK))?;
    let mut cells = (0..count)
        .map(|index| {
            let cell = notebook
                .subprojection(&format!("/cells/{index}"))
                .map_err(|_| notebook_error(INVALID_NOTEBOOK))?;
            if cell.object_entries().is_none() {
                return Err(notebook_error(INVALID_NOTEBOOK));
            }
            joined_cell_source(&cell)?;
            Ok(cell)
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    let cell_id = cell_id.filter(|id| !id.is_empty());
    let index = match cell_id {
        None if edit_mode == EDIT_MODE_INSERT => 0,
        None => {
            return Err(notebook_error(
                "Cell ID must be specified when not inserting a new cell.",
            ))
        }
        Some(id) => {
            let found = cells
                .iter()
                .position(|cell| cell.string_at("/id") == Some(id));
            let index = if let Some(index) = found {
                index
            } else if let Some(index) = parse_cell_id(id) {
                if index >= count {
                    return Err(notebook_error(format!(
                        "Cell with index {index} does not exist in notebook."
                    )));
                }
                index
            } else {
                return Err(notebook_error(format!(
                    "Cell with ID \"{id}\" not found in notebook."
                )));
            };
            index + usize::from(edit_mode == EDIT_MODE_INSERT)
        }
    };
    let new_source = input
        .subprojection("/new_source")
        .ok()
        .filter(|source| source.string_units("").is_some())
        .unwrap_or_else(|| "".into());
    let language = notebook
        .subprojection("/metadata/language_info/name")
        .ok()
        .filter(|name| !name.at_pointer("").is_some_and(|value| value.is_null()))
        .unwrap_or_else(|| "python".into());
    let supports_ids = notebook.number_at("/nbformat").is_some_and(|format| {
        format > 4.0
            || (format == 4.0
                && notebook
                    .number_at("/nbformat_minor")
                    .is_some_and(|minor| minor >= 5.0))
    });
    let result_id = if edit_mode == EDIT_MODE_INSERT {
        supports_ids.then(|| insert_id().chars().take(8).collect::<String>())
    } else {
        cell_id.map(str::to_owned)
    };
    let old_source = match edit_mode {
        EDIT_MODE_DELETE => Some(joined_cell_source(&cells.remove(index))?),
        EDIT_MODE_INSERT => {
            let kind = cell_type.unwrap_or("code");
            let mut fields = vec![("cell_type".into(), kind.into())];
            if let Some(id) = &result_id {
                fields.push(("id".into(), id.as_str().into()));
            }
            fields.extend([
                ("source".into(), new_source.clone()),
                ("metadata".into(), crate::generated_result(&json!({}))),
            ]);
            if kind != "markdown" {
                fields.extend([
                    (
                        "execution_count".into(),
                        crate::generated_result(&Value::Null),
                    ),
                    ("outputs".into(), crate::generated_result(&json!([]))),
                ]);
            }
            cells.insert(
                index,
                Utf16JsonProjection::object(fields)
                    .map_err(|error| notebook_error(error.to_string()))?,
            );
            None
        }
        _ => {
            let cell = &mut cells[index];
            let old_source = joined_cell_source(cell)?;
            if cell.string_at("/cell_type") == Some("code") {
                cell.set_field("execution_count", crate::generated_result(&Value::Null))
                    .map_err(|error| notebook_error(error.to_string()))?;
                cell.set_field("outputs", crate::generated_result(&json!([])))
                    .map_err(|error| notebook_error(error.to_string()))?;
            }
            cell.set_field("source", new_source.clone())
                .map_err(|error| notebook_error(error.to_string()))?;
            if let Some(kind) = cell_type {
                cell.set_field("cell_type", kind.into())
                    .map_err(|error| notebook_error(error.to_string()))?;
            }
            Some(old_source)
        }
    };
    notebook
        .set_field(
            "cells",
            Utf16JsonProjection::array(cells).map_err(|error| notebook_error(error.to_string()))?,
        )
        .map_err(|error| notebook_error(error.to_string()))?;
    let display_id = result_id.as_deref().unwrap_or("undefined");
    let prefix = match edit_mode {
        EDIT_MODE_DELETE => format!("Deleted cell {display_id}"),
        EDIT_MODE_INSERT if result_id.is_none() => "Inserted a new cell with ".into(),
        EDIT_MODE_INSERT => format!("Inserted cell {display_id} with "),
        _ => format!("Updated cell {display_id} with "),
    };
    let mut units: Vec<u16> = prefix.encode_utf16().collect();
    if edit_mode != EDIT_MODE_DELETE {
        units.extend(new_source.string_units("").unwrap_or_default());
    }
    let model_content = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .map_err(|error| notebook_error(error.to_string()))?;
    Ok(NotebookMutation {
        new_source,
        old_source,
        language,
        cell_id: result_id,
        model_content,
    })
}

fn serialize_notebook(notebook: &Utf16JsonProjection) -> Result<String, ToolError> {
    let pretty = notebook
        .to_pretty_json_string()
        .map_err(|error| ToolError::Io(error.to_string().into()))?;
    Ok(pretty
        .split('\n')
        .map(|line| {
            let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
            format!("{}{}", " ".repeat(spaces / 2), &line[spaces..])
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn supports_persistent_chat(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("edit Jupyter notebook cells (.ipynb)".into())
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
        // Verbatim claude-code v2.1.183 NotebookEdit description const `f2a`
        // (em-dash is U+2014).
        "Edit a cell in a Jupyter notebook — replace, insert, or delete.".to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
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
- `edit_mode` defaults to `replace`. Use `insert` to add a new cell after the cell with the given `cell_id` (or at the beginning of the notebook if `cell_id` is omitted) — `cell_type` is required when inserting. Use `delete` to remove the cell."
            .into()
    }

    fn get_path(&self, input: &Utf16JsonProjection) -> Option<PathBuf> {
        crate::input_string(input, "notebook_path").map(PathBuf::from)
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = tool_api::util::ids::ulid_or_uuid();
        let notebook_path = crate::input_string(&input, "notebook_path")
            .ok_or_else(|| ToolError::InvalidInput("notebook_path is required".into()))?;
        // `cell_id` and `edit_mode` are optional per the TS schema
        // (`NotebookEditTool.ts:37-55`): `cell_id` absent means "insert at the
        // beginning"; `edit_mode` defaults to `replace`. `cell_type` is the
        // newly-honored insert/replace cell kind.
        let cell_id = crate::input_string(&input, "cell_id");
        let edit_mode = crate::input_string(&input, "edit_mode").unwrap_or(EDIT_MODE_REPLACE);
        let cell_type =
            crate::input_string(&input, "cell_type").map(std::string::ToString::to_string);

        let requested_path = crate::checked_model_file_path(notebook_path, &self.ctx.cwd())?;
        let path = requested_path.clone();
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
        // Native NotebookEdit validates the extension case-sensitively.
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("ipynb") {
            self.emit_failed(&invocation_id, "not_a_notebook").await;
            return Err(ToolError::InvalidInput(
                "File must be a Jupyter notebook (.ipynb file). For editing other file types, use the FileEdit tool.".into(),
            ));
        }

        if edit_mode == EDIT_MODE_INSERT && cell_type.is_none() {
            self.emit_failed(&invocation_id, "cell_type_required").await;
            return Err(ToolError::InvalidInput(
                "Cell type is required when using edit_mode=insert.".into(),
            ));
        }

        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, requested_path.clone(), true) {
            Ok(path) => path,
            Err(message) => {
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::InvalidInput(message.into()));
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

        let current_mtime_ms = tokio::fs::metadata(&canon)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        // NotebookEdit's guard has no full-content equality fallback. Even a
        // byte-identical file with an advanced mtime must be read again.
        let entry = tool_api::read_file_state::get(&self.ctx.read_file_state, &requested_path)
            .ok_or_else(|| ToolError::InvalidInput(crate::FILE_NOT_READ_ERROR.into()))?;
        if current_mtime_ms > entry.mtime_ms {
            return Err(ToolError::InvalidInput(
                crate::FILE_UNEXPECTEDLY_MODIFIED_ERROR.into(),
            ));
        }

        if tokio::fs::metadata(&canon)
            .await
            .is_ok_and(|metadata| metadata.len() > MAX_NOTEBOOK_BYTES as u64)
        {
            return Err(ToolError::InvalidInput(
                crate::notebook_read::notebook_too_large_message().into(),
            ));
        }
        let bytes = match tokio::fs::read(&canon).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::InvalidInput(
                    "Notebook file does not exist.".into(),
                ));
            }
            Err(error) => return Err(ToolError::Io(error.to_string().into())),
        };
        if bytes.len() > MAX_NOTEBOOK_BYTES {
            return Err(ToolError::InvalidInput(
                crate::notebook_read::notebook_too_large_message().into(),
            ));
        }
        if let Some(kind) = crate::file_meta::classify_not_text(&bytes) {
            return Err(ToolError::InvalidInput(
                kind.mutation_refusal_message().into(),
            ));
        }
        let (raw, encoding, line_ending) = crate::file_meta::read_with_metadata(&bytes);
        let mut notebook = Utf16JsonProjection::parse(&raw)
            .map_err(|_| ToolError::InvalidInput("Notebook is not valid JSON.".into()))?;
        let root = notebook.at_pointer("").expect("parsed JSON has a root");
        if root.is_null()
            || root.as_bool() == Some(false)
            || root.as_number() == Some(0.0)
            || root.code_units().is_some_and(<[u16]>::is_empty)
        {
            return Err(notebook_error("Notebook is not valid JSON."));
        }
        let mutation = mutate_notebook(
            &mut notebook,
            &input,
            cell_id,
            edit_mode,
            cell_type.as_deref(),
            || uuid::Uuid::new_v4().to_string(),
        )?;
        // The flat JSON stringifier preserves ECMAScript key order, numeric
        // spelling, and lone UTF-16 surrogates. Halving its structural indent
        // implements JSON.stringify(notebook, null, 1).
        let serialized = serialize_notebook(&notebook)?;
        let output = crate::file_meta::encode_with_metadata(&serialized, encoding, line_ending);
        if output.len() > MAX_NOTEBOOK_BYTES {
            return Err(ToolError::InvalidInput(
                "Edit rejected: the updated notebook would exceed the maximum size this tool can read (100MB). The file was left unchanged.".into(),
            ));
        }
        if let Some(history) = ctx.file_history.as_ref() {
            history.track_edit(&canon.to_string_lossy()).await;
        }
        tokio::fs::write(&canon, output)
            .await
            .map_err(|error| ToolError::Io(error.to_string().into()))?;

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
        tool_api::read_file_state::set_with_requested_aliases(
            &self.ctx.read_file_state,
            requested_path.clone(),
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
            true,
            vec![requested_path.clone(), path.clone()],
        );

        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, 1, duration_ms).await;

        let mut fields = vec![("new_source".to_string(), mutation.new_source.clone())];
        if let Some(old_source) = mutation.old_source {
            fields.push(("old_source".into(), old_source));
        }
        fields.extend([
            (
                "cell_type".into(),
                cell_type.as_deref().unwrap_or("code").into(),
            ),
            ("language".into(), mutation.language),
            ("edit_mode".into(), edit_mode.into()),
        ]);
        if let Some(id) = mutation.cell_id {
            fields.push(("cell_id".into(), id.into()));
        }
        fields.extend([
            ("error".into(), "".into()),
            (
                "notebook_path".into(),
                requested_path.to_string_lossy().as_ref().into(),
            ),
            ("original_file".into(), raw.into()),
            ("updated_file".into(), serialized.into()),
        ]);
        let data = Utf16JsonProjection::object(fields)
            .map_err(|error| ToolError::Io(error.to_string().into()))?;
        Ok(ToolCallResult {
            data,
            model_content: Some(mutation.model_content),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn checked_source(
        source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> (String, Vec<u16>) {
        let display = source
            .string_at("")
            .expect("error source is a root string")
            .to_owned();
        let units = source
            .string_units("")
            .expect("error source retains UTF-16 units");
        assert_eq!(String::from_utf16_lossy(&units), display);
        (display, units)
    }

    fn source_text(source: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> String {
        checked_source(source).0
    }

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
    /// file's current content + floored mtime under Native's normalized
    /// session-cwd route key with `offset`/`limit` = `None`. Canonical remains
    /// for disk access only.
    ///
    /// These focused guard tests seed state directly. The current production
    /// Read-to-NotebookEdit path is exercised separately below.
    fn seed_full_read(ctx: &BuiltinToolContext, target: &std::path::Path) {
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let canon = std::fs::canonicalize(target).unwrap();
        let content = std::fs::read_to_string(&canon).unwrap();
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

    #[test]
    fn native_322_notebook_mutation_fixtures() {
        let fixture = Utf16JsonProjection::parse(include_str!(
            "../../../../scripts/tests/fixtures/file_notebook_path_322.json"
        ))
        .unwrap();
        for index in 0..fixture.array_len("/notebooks").unwrap() {
            let case = fixture
                .subprojection(&format!("/notebooks/{index}"))
                .unwrap();
            let name = case.string_at("/name").unwrap();
            let mut notebook =
                Utf16JsonProjection::parse(case.string_at("/notebookJson").unwrap()).unwrap();
            let input = case.subprojection("/input").unwrap();
            let result = mutate_notebook(
                &mut notebook,
                &input,
                crate::input_string(&input, "cell_id"),
                crate::input_string(&input, "edit_mode").unwrap_or("replace"),
                crate::input_string(&input, "cell_type"),
                || "native123456".into(),
            );
            if let Some(expected) = case.string_at("/expected/error") {
                assert_eq!(
                    result.err().unwrap().model_facing_message().string_at(""),
                    Some(expected),
                    "{name}"
                );
                continue;
            }
            let result = result.unwrap_or_else(|error| panic!("{name}: {error:?}"));
            let mut fields = vec![("new_source".into(), result.new_source)];
            if let Some(old) = result.old_source {
                fields.push(("old_source".into(), old));
            }
            fields.extend([
                (
                    "cell_type".into(),
                    crate::input_string(&input, "cell_type")
                        .unwrap_or("code")
                        .into(),
                ),
                ("language".into(), result.language),
                (
                    "edit_mode".into(),
                    crate::input_string(&input, "edit_mode")
                        .unwrap_or("replace")
                        .into(),
                ),
            ]);
            if let Some(id) = result.cell_id {
                fields.push(("cell_id".into(), id.into()));
            }
            fields.push(("error".into(), "".into()));
            assert_eq!(
                Utf16JsonProjection::object(fields)
                    .unwrap()
                    .to_json_string()
                    .unwrap(),
                case.subprojection("/expected/data")
                    .unwrap()
                    .to_json_string()
                    .unwrap(),
                "{name}"
            );
            assert_eq!(
                serialize_notebook(&notebook).unwrap(),
                case.string_at("/expected/updatedFile").unwrap(),
                "{name}"
            );
            assert_eq!(
                result.model_content.string_units(""),
                case.string_units("/expected/model"),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn native_322_notebook_validation_fixtures() {
        let fixture = Utf16JsonProjection::parse(include_str!(
            "../../../../scripts/tests/fixtures/file_notebook_path_322.json"
        ))
        .unwrap();
        for index in 0..fixture.array_len("/validations").unwrap() {
            let case = fixture
                .subprojection(&format!("/validations/{index}"))
                .unwrap();
            let tmp = TempDir::new().unwrap();
            let filename = std::path::Path::new(case.string_at("/configuration/file").unwrap())
                .file_name()
                .unwrap();
            let target = tmp.path().join(filename);
            let raw = case.string_at("/configuration/raw").unwrap();
            std::fs::write(&target, raw).unwrap();
            let (ctx, _) = make_ctx(&tmp);
            if case
                .at_pointer("/configuration/read")
                .and_then(|value| value.as_bool())
                == Some(true)
            {
                seed_full_read(&ctx, &target);
                let key = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
                let mut entry = tool_api::read_file_state::get(&ctx.read_file_state, &key).unwrap();
                let timestamp = case.number_at("/configuration/timestamp").unwrap() as i64;
                let mtime = case.number_at("/configuration/mtime").unwrap() as i64;
                entry.mtime_ms += timestamp - mtime;
                tool_api::read_file_state::set(&ctx.read_file_state, key, entry);
            }
            let error = NotebookEditTool::new(ctx).call(crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(), "cell_id": "c1", "new_source": "x"
            })), fresh_ctx(), fresh_tx()).await.unwrap_err();
            assert_eq!(
                error.model_facing_message().string_at(""),
                case.string_at("/expected/message"),
                "{}",
                case.string_at("/name").unwrap()
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), raw);
        }
    }

    #[tokio::test]
    async fn native_322_notebook_input_size_guard_precedes_loading() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("large.ipynb");
        std::fs::File::create(&target)
            .unwrap()
            .set_len(MAX_NOTEBOOK_BYTES as u64 + 1)
            .unwrap();
        let (ctx, _) = make_ctx(&tmp);
        tool_api::read_file_state::set(
            &ctx.read_file_state,
            target.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: String::new(),
                mtime_ms: i64::MAX,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        let error = NotebookEditTool::new(ctx)
            .call(
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(), "cell_id": "c1", "new_source": "x"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.model_facing_message().string_at(""),
            Some(crate::notebook_read::notebook_too_large_message().as_str())
        );
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
                crate::generated_result(&json!({ "file_path": "nb.ipynb" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Read must handle .ipynb");
        // No seed_full_read: the Read above must be sufficient.
        let tool = NotebookEditTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(&json!({
                    "notebook_path": "nb.ipynb",
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "print('via-real-read')"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("Read→NotebookEdit must satisfy the staleness guard");
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()),
            Some("Updated cell c1 with print('via-real-read')")
        );
    }

    #[tokio::test]
    async fn relative_notebook_path_uses_session_cwd_and_records_the_absolute_route() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("relative.ipynb");
        std::fs::write(&target, sample_notebook()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        assert_ne!(
            ctx.cwd(),
            std::env::current_dir().unwrap(),
            "the test must distinguish the session cwd from the process cwd"
        );
        seed_full_read(&ctx, &target);
        let tool = NotebookEditTool::new(ctx.clone());

        // The schema accepts this string and NotebookEdit has no validator
        // rejecting relative paths. Its session cwd, not the process cwd, is
        // the path base (the same rule as Read/Edit/Write).
        let result = tool
            .call(
                crate::generated_result(&json!({
                    "notebook_path": "relative.ipynb",
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "print('session-cwd')"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("relative NotebookEdit path resolves in the active session cwd");
        assert!(crate::test_model_text(result.model_content.as_ref())
            .unwrap()
            .contains("session-cwd"));

        let key = crate::normalize_model_file_path("relative.ipynb", &ctx.cwd());
        let routes = ctx
            .read_file_state
            .lock()
            .unwrap()
            .requested_path_groups(&key);
        assert!(
            routes
                .iter()
                .any(|route| route == std::slice::from_ref(&target)),
            "the model route is captured as the resolved session-cwd spelling: {routes:?}"
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "print('world')"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Model-facing string rides on model_content (the binary result `data` is
        // the {new_source, old_source, …} record — no `content` field).
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()),
            Some("Updated cell c1 with print('world')")
        );
        assert_eq!(
            crate::test_data(&result.data)["new_source"],
            "print('world')"
        );
        assert_eq!(
            crate::test_data(&result.data)["old_source"],
            "print('hello')"
        );
        assert_eq!(crate::test_data(&result.data)["edit_mode"], "replace");
        assert_eq!(crate::test_data(&result.data)["error"], "");
        assert!(crate::test_data(&result.data).get("content").is_none());
        assert!(crate::test_data(&result.data)["updated_file"].is_string());
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
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "replace",
                "new_source": "print('world')"
            })),
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "delete"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Model-facing string rides on model_content; data is the edit record.
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()),
            Some("Deleted cell c1")
        );
        assert_eq!(crate::test_data(&result.data)["edit_mode"], "delete");
        assert_eq!(
            crate::test_data(&result.data)["old_source"],
            "print('hello')"
        );
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "insert",
                    "cell_type": "code",
                    "new_source": "print('inserted')"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // The message renders the FRESH cell id (NB.2 / NotebookEditTool.ts:
        // 152-156 with cell_id = new_cell_id), so only the static framing is
        // byte-fixed.
        let content = crate::test_model_text(result.model_content.as_ref()).unwrap();
        assert!(content.starts_with("Inserted cell "));
        assert!(content.ends_with(" with print('inserted')"));
        // Insert has no prior cell → old_source omitted (binary `void 0`).
        assert!(crate::test_data(&result.data).get("old_source").is_none());
        assert_eq!(crate::test_data(&result.data)["edit_mode"], "insert");
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
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "replace",
                "new_source": "print('world')"
            })),
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
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c2",
                "edit_mode": "replace",
                "new_source": "## Updated"
            })),
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "insert",
                    "cell_type": "code",
                    "new_source": src
                })),
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
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "cell-0",
                "edit_mode": "replace",
                "new_source": "print('by index')"
            })),
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "cell-99",
                    "edit_mode": "replace",
                    "new_source": "x"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(source_text(&err.model_facing_message()).contains("does not exist in notebook"));
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
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "insert",
                "cell_type": "markdown",
                "new_source": "# inserted md"
            })),
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "insert",
                    "new_source": "x"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // Byte-faithful to NotebookEditTool.ts:213.
        assert_eq!(
            source_text(&err.model_facing_message()),
            "Cell type is required when using edit_mode=insert."
        );
    }

    // ───────────────────────── NB.5: schema + insert-at-0 ───────────────────

    #[test]
    fn schema_requiredness_matches_ts() {
        assert_eq!(INPUT_SCHEMA.array_len("/required"), Some(2));
        assert_eq!(INPUT_SCHEMA.string_at("/required/0"), Some("notebook_path"));
        assert_eq!(INPUT_SCHEMA.string_at("/required/1"), Some("new_source"));
        // cell_type is now a known property (enum code|markdown).
        assert_eq!(
            INPUT_SCHEMA.string_at("/properties/cell_type/enum/0"),
            Some("code")
        );
        assert_eq!(
            INPUT_SCHEMA.string_at("/properties/cell_type/enum/1"),
            Some("markdown")
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
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "edit_mode": "insert",
                "cell_type": "code",
                "new_source": "print('first cell now')"
            })),
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "edit_mode": "replace",
                    "new_source": "x"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // Byte-faithful to NotebookEditTool.ts:264.
        assert!(source_text(&err.model_facing_message())
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "delete"
                })),
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
    async fn invalid_json_is_a_hard_error_before_mutation() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bad.ipynb");
        std::fs::write(&target, "this is not json").unwrap();
        let (ctx, _) = make_ctx(&tmp);
        seed_full_read(&ctx, &target);
        let error = NotebookEditTool::new(ctx)
            .call(
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1", "edit_mode": "delete", "new_source": ""
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.model_facing_message().string_at(""),
            Some("Notebook is not valid JSON.")
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "this is not json");
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "x"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(m) => assert_eq!(
                m.string_at(""),
                Some(
                    "File must be a Jupyter notebook (.ipynb file). For editing other file types, use the FileEdit tool."
                )
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "c1",
                    "edit_mode": "replace",
                    "new_source": "print('blocked')"
                })),
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
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c1",
                "edit_mode": "replace",
                "new_source": "print('first')"
            })),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // No re-seed: the post-write set from the first edit satisfies the guard.
        tool.call(
            crate::generated_result(&json!({
                "notebook_path": target.to_str().unwrap(),
                "cell_id": "c2",
                "edit_mode": "replace",
                "new_source": "## Updated"
            })),
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
                crate::generated_result(&json!({
                    "notebook_path": target.to_str().unwrap(),
                    "cell_id": "nonexistent",
                    "edit_mode": "replace",
                    "new_source": "x"
                })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(source_text(&err.model_facing_message()).contains("not found"));
    }
}
