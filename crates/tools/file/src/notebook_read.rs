//! Structured `.ipynb` reading for [`crate::read::FileReadTool`].
//!
//! 1:1 port of claude-code's notebook read pipeline
//! (`utils/notebook.ts` `readNotebook` → `processCell` → `processOutput`,
//! and `FileReadTool.ts:822-863` `callInner`'s `ext === 'ipynb'` branch).
//!
//! A notebook is JSON with `{ metadata, cells: [...] }`. `read_notebook`
//! parses it and emits an ARRAY of structured cells — NOT plain text — so the
//! model sees `{ cellType, source, execution_count?, cell_id, language?,
//! outputs? }` per cell, exactly as the TS `NotebookCellSource` shape. Field
//! names + ordering match the TS object literals byte-for-byte (verified
//! against `notebook.ts:83-117` / `:59-81`).
//!
//! Output text is truncated through the same Bash `formatOutput` cap the TS
//! uses (`processOutputText` → `formatOutput`), so a runaway cell output is
//! elided with the byte-locked `\n\n... [{N} lines truncated] ...` suffix.

use serde_json::{json, Map, Value};

/// Bash output cap reused by `processOutputText` (TS `formatOutput` →
/// `getMaxOutputLength()` default). Mirrors `tool-shell`'s
/// `BASH_MAX_OUTPUT_DEFAULT` (30_000) — duplicated here to avoid a crate
/// dependency edge from `tool-file` onto `tool-shell` (and the env override is
/// a Bash-only concern; notebook reads always use the default cap).
const NOTEBOOK_OUTPUT_MAX_LENGTH: usize = 30_000;

/// Threshold above which a code cell's combined outputs are replaced with a
/// "too large" stub (`notebook.ts:20` `LARGE_OUTPUT_THRESHOLD`).
const LARGE_OUTPUT_THRESHOLD: usize = 10_000;

/// `Read` tool name used in the "outputs too large" hint (the TS hint reads
/// `cat <path> | jq …`; the leading verb is the Bash tool name). Byte-locked to
/// claude-code's `BASH_TOOL_NAME` so the hint matches.
const BASH_TOOL_NAME: &str = "Bash";

/// Count newline characters in `s` starting at byte index `from`.
// A plain byte filter mirrors `read.rs`'s newline count; the `bytecount` crate
// is not a dependency of this crate (and adding one is out of scope).
#[allow(clippy::naive_bytecount)]
fn count_newlines_from(s: &str, from: usize) -> usize {
    s.as_bytes()[from.min(s.len())..]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// `formatOutput`'s text branch (TS `BashTool/utils.ts:133-165`), limited to
/// the non-image path (notebook output text is never an image data-URI). When
/// `content` fits under the cap it is returned verbatim; otherwise it is
/// sliced to `NOTEBOOK_OUTPUT_MAX_LENGTH` bytes and the byte-locked
/// `\n\n... [{N} lines truncated] ...` suffix is appended, where `{N}` is the
/// newline count in the elided tail plus one (TS
/// `countCharInString(content, '\n', maxOutputLength) + 1`).
///
/// The slice is taken on a UTF-8 char boundary at or below the cap so the
/// result is always valid UTF-8 (TS slices by UTF-16 code unit; for the ASCII-
/// dominant cell outputs this matches, and a boundary-safe truncation can only
/// keep fewer bytes — never more — which is the conservative direction).
fn format_output_text(content: &str) -> String {
    if content.len() <= NOTEBOOK_OUTPUT_MAX_LENGTH {
        return content.to_string();
    }
    // Largest char boundary <= the cap.
    let mut cut = NOTEBOOK_OUTPUT_MAX_LENGTH;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    let remaining_lines = count_newlines_from(content, cut) + 1;
    format!(
        "{}\n\n... [{remaining_lines} lines truncated] ...",
        &content[..cut]
    )
}

/// `processOutputText` (TS `notebook.ts:34-39`): a `string | string[] |
/// undefined` source is joined (arrays concatenated with no separator), then
/// passed through [`format_output_text`]. An absent/`null` source yields `""`.
fn process_output_text(text: Option<&Value>) -> String {
    // Only a string or an array-of-strings carries text; every other JSON
    // shape (absent / null / number / object) is empty per TS's `!text` falsy
    // guard (numbers/objects never appear in nbformat output text).
    let raw = match text {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .concat(),
        _ => return String::new(),
    };
    if raw.is_empty() {
        return String::new();
    }
    format_output_text(&raw)
}

/// `extractImage` (TS `notebook.ts:41-57`): pull a base64 PNG or JPEG out of an
/// output's `data` bag, stripping all whitespace from the payload (TS
/// `.replace(/\s/g, '')`). PNG takes precedence over JPEG. Returns the
/// `{ image_data, media_type }` object, or `None` when neither key is a string.
fn extract_image(data: &Map<String, Value>) -> Option<Value> {
    for (key, media) in [("image/png", "image/png"), ("image/jpeg", "image/jpeg")] {
        if let Some(s) = data.get(key).and_then(Value::as_str) {
            let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
            return Some(json!({ "image_data": cleaned, "media_type": media }));
        }
    }
    None
}

/// Approximate byte size of a processed output for the large-output guard,
/// mirroring TS `isLargeOutputs` (`notebook.ts:22-32`): `text.length +
/// image.image_data.length` summed across outputs.
fn output_size(processed: &Value) -> usize {
    let text_len = processed
        .get("text")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    let image_len = processed
        .get("image")
        .and_then(|i| i.get("image_data"))
        .and_then(Value::as_str)
        .map_or(0, str::len);
    text_len + image_len
}

/// `processOutput` (TS `notebook.ts:59-81`): map one raw nbformat output to the
/// structured `{ output_type, text, image? }` shape. Unknown `output_type`s
/// yield `None` (TS's switch has no default → `undefined`, dropped downstream).
/// Field ordering matches the TS object literals.
fn process_output(output: &Value) -> Option<Value> {
    let output_type = output.get("output_type").and_then(Value::as_str)?;
    match output_type {
        "stream" => {
            let mut m = Map::new();
            m.insert("output_type".into(), json!(output_type));
            m.insert(
                "text".into(),
                json!(process_output_text(output.get("text"))),
            );
            Some(Value::Object(m))
        }
        "execute_result" | "display_data" => {
            let data = output.get("data");
            let text = data
                .and_then(|d| d.get("text/plain"))
                .map_or_else(String::new, |t| process_output_text(Some(t)));
            let mut m = Map::new();
            m.insert("output_type".into(), json!(output_type));
            m.insert("text".into(), json!(text));
            // TS always sets the `image` key (`output.data && extractImage(...)`);
            // it is `undefined` (dropped by JSON.stringify) when there is no
            // image. We only insert the key when an image is present so the
            // serialized shape matches (an absent image => no `image` key).
            if let Some(obj) = data.and_then(Value::as_object) {
                if let Some(img) = extract_image(obj) {
                    m.insert("image".into(), img);
                }
            }
            Some(Value::Object(m))
        }
        "error" => {
            let ename = output.get("ename").and_then(Value::as_str).unwrap_or("");
            let evalue = output.get("evalue").and_then(Value::as_str).unwrap_or("");
            let traceback = output
                .get("traceback")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let combined = format!("{ename}: {evalue}\n{traceback}");
            let mut m = Map::new();
            m.insert("output_type".into(), json!(output_type));
            m.insert("text".into(), json!(format_output_text(&combined)));
            Some(Value::Object(m))
        }
        _ => None,
    }
}

/// `processCell` (TS `notebook.ts:83-117`): map one raw notebook cell to the
/// structured `NotebookCellSource` shape. `index` is the 0-based cell position
/// (for the `cell-N` id fallback + the large-output hint), `code_language` is
/// the notebook's `metadata.language_info.name` (default `"python"`).
///
/// Field insertion order matches the TS literal: `cellType`, `source`,
/// `execution_count`, `cell_id`, then `language` (code only), then `outputs`
/// (code with outputs only). `execution_count` is only emitted for code cells
/// with a truthy count (TS `cell.execution_count || undefined`), so a `null` or
/// `0` count is dropped.
fn process_cell(cell: &Value, index: usize, code_language: &str) -> Value {
    let cell_type = cell.get("cell_type").and_then(Value::as_str).unwrap_or("");
    let is_code = cell_type == "code";

    // `source` join (TS `Array.isArray ? join('') : source`).
    let source = match cell.get("source") {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .concat(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };

    let cell_id = cell
        .get("id")
        .and_then(Value::as_str)
        .map_or_else(|| format!("cell-{index}"), str::to_string);

    let mut m = Map::new();
    m.insert("cellType".into(), json!(cell_type));
    m.insert("source".into(), json!(source));

    // `execution_count: cell.execution_count || undefined` for code cells only;
    // `undefined` for non-code. Emit the key only for a code cell with a truthy
    // integer count (matches TS, where `undefined` is dropped by stringify).
    if is_code {
        if let Some(n) = cell.get("execution_count").and_then(Value::as_i64) {
            if n != 0 {
                m.insert("execution_count".into(), json!(n));
            }
        }
    }

    m.insert("cell_id".into(), json!(cell_id));

    // `language` only on code cells (TS skips it for text cells so they don't
    // inherit the code language).
    if is_code {
        m.insert("language".into(), json!(code_language));
    }

    // `outputs` only when a code cell has a non-empty `outputs` array.
    if is_code {
        if let Some(raw_outputs) = cell.get("outputs").and_then(Value::as_array) {
            if !raw_outputs.is_empty() {
                let processed: Vec<Value> = raw_outputs.iter().filter_map(process_output).collect();
                // Large-output guard (TS `processCell:104-113` `!includeLargeOutputs
                // && isLargeOutputs(outputs)`): the non-cellId read path passes
                // `includeLargeOutputs = false`, so over-threshold combined outputs
                // collapse to a single stream stub pointing at jq.
                let total: usize = processed.iter().map(output_size).sum();
                let outputs = if total > LARGE_OUTPUT_THRESHOLD {
                    json!([{
                        "output_type": "stream",
                        "text": format!(
                            "Outputs are too large to include. Use {BASH_TOOL_NAME} with: cat <notebook_path> | jq '.cells[{index}].outputs'"
                        ),
                    }])
                } else {
                    Value::Array(processed)
                };
                m.insert("outputs".into(), outputs);
            }
        }
    }

    Value::Object(m)
}

/// `aV` (cc-238.js @289991xxx, `var aV=104857600`) — the hard byte cap `K0f`
/// applies to a notebook BEFORE parsing it, both to `stat().size` and to the
/// `aV+1` probe read. Absent from 2.1.220 (`Notebook file exceeds the maximum
/// size` → 0 hits in 220, 2 in 238).
pub const MAX_NOTEBOOK_READ_SIZE: u64 = 104_857_600;

/// `K0f`'s first guard (cc-238.js @289994340):
/// `if(!n.isFile()&&!n.isDirectory())throw new Wur(…)`. New in 2.1.238
/// (0 hits in 2.1.220). Note that a DIRECTORY passes this leg upstream — the
/// subsequent read is what fails — so the port mirrors the same disjunction.
pub const NOTEBOOK_NOT_REGULAR_FILE: &str =
    "Notebook path is not a regular file (device, FIFO, or socket).";

/// `$Ka()`'s POSIX branch (cc-238.js @289991300), shared by
/// [`notebook_too_large_message`] and the Read tool's "Notebook content (…)
/// exceeds maximum allowed size (…)" error.
///
/// Two things to know before touching these bytes:
///   * 2.1.238 de-parameterised the PATH — 2.1.220 interpolated the real,
///     QUOTED path (`cat "${t}" | jq …`, 220 @109111686); 238 emits the literal
///     placeholder `cat <notebook_path>` unquoted (238 @113219062).
///   * `Use ${Oi} with jq` is an interpolation SLOT (`Oi="Bash"`), not a
///     literal. LingXi renders that slot as "a registered shell tool" repo-wide
///     because it registers Bash AND PowerShell — an intentional, test-pinned
///     divergence (`grep.rs::description_is_byte_faithful`). Do NOT "align" it.
pub const NOTEBOOK_JQ_GUIDANCE: &str = "Use a registered shell tool with jq to read specific portions:\n  cat <notebook_path> | jq '.cells[:20]' # First 20 cells\n  cat <notebook_path> | jq '.cells[100:120]' # Cells 100-120\n  cat <notebook_path> | jq '.cells | length' # Count total cells\n  cat <notebook_path> | jq '.cells[] | select(.cell_type==\"code\") | .source' # All code sources";

/// `l8n()` (cc-238.js @289991xxx):
/// ``Notebook file exceeds the maximum size this tool can read (${Ba(aV)}). ${$Ka()}``.
/// `Ba(104857600)` renders `100MB` (verified against the oracle's `Ba`
/// @281639906 and the port's [`crate::read::format_file_size`]).
#[must_use]
pub fn notebook_too_large_message() -> String {
    format!(
        "Notebook file exceeds the maximum size this tool can read ({}). {NOTEBOOK_JQ_GUIDANCE}",
        crate::read::format_file_size(MAX_NOTEBOOK_READ_SIZE),
    )
}

/// Byte-locked `$0i` (cc-238.js @289994340, live JS; string table @74017843) —
/// the message `K0f` throws when [`is_valid_notebook_shape`] refuses. 2.1.220
/// carried the shorter `…array of cell objects).` form (0 hits in 238, 2 hits
/// in 220); 238 appended the `source` clause (2 hits in 238, 0 in 220).
pub const INVALID_NOTEBOOK_SHAPE: &str = "Notebook file is not a valid Jupyter notebook (top-level \"cells\" must be an array of cell objects, each with a string or string-array \"source\").";

/// One cell of `X2t` (cc-238.js @289990631):
///
/// ```js
/// r===null||typeof r!=="object"
///   ||typeof r.source!=="string"
///      &&!(Array.isArray(r.source)&&r.source.every((n)=>typeof n==="string"))
/// ```
///
/// i.e. a cell must be a non-null object whose `source` is either a string or
/// an array of strings (an EMPTY array passes — JS `[].every` is `true`). A
/// missing `source` is `undefined`, which is neither, so it fails. 2.1.220 only
/// had the null/object half; 2.1.238 added the `source` type check. LingXi had
/// neither before FT-02.
fn is_valid_cell(cell: &Value) -> bool {
    let Some(obj) = cell.as_object() else {
        // Covers JS's `r===null` and `typeof r!=="object"`. A JSON array cell
        // is `typeof "object"` upstream but then fails the `source` leg, so
        // rejecting it here reaches the same verdict.
        return false;
    };
    match obj.get("source") {
        Some(Value::String(_)) => true,
        Some(Value::Array(items)) => items.iter().all(Value::is_string),
        _ => false,
    }
}

/// `X2t` (cc-238.js @289990631): a value is a readable notebook iff its `cells`
/// is an array and every element passes [`is_valid_cell`].
#[must_use]
pub fn is_valid_notebook_shape(notebook: &Value) -> bool {
    notebook
        .get("cells")
        .and_then(Value::as_array)
        .is_some_and(|cells| cells.iter().all(is_valid_cell))
}

/// Parse + process a whole notebook into structured cells — 1:1 with TS
/// `readNotebook(path)` (no `cellId`, so every cell is processed with
/// `includeLargeOutputs = false`). Returns the cells array on success, or an
/// error string when the bytes are not a JSON object with a `cells` array.
///
/// `raw` is the file's UTF-8 text (already decoded by the caller). The language
/// is read from `metadata.language_info.name`, defaulting to `"python"`.
pub fn read_notebook(raw: &str) -> Result<Vec<Value>, String> {
    // Binary `readNotebook` (atl): byte-locked invalid-JSON error.
    let notebook: Value = serde_json::from_str(raw).map_err(|e| {
        format!("Notebook file is not valid JSON (it may be truncated, corrupted, or still being written): {e}")
    })?;
    let language = notebook
        .get("metadata")
        .and_then(|m| m.get("language_info"))
        .and_then(|li| li.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("python")
        .to_string();
    // Binary `readNotebook` (`K0f`): `if(!X2t(s))throw new Wur($0i)` — the
    // shape gate is [`is_valid_notebook_shape`], NOT merely "cells is an array".
    let cells = notebook
        .get("cells")
        .and_then(Value::as_array)
        .filter(|cells| cells.iter().all(is_valid_cell))
        .ok_or_else(|| INVALID_NOTEBOOK_SHAPE.to_string())?;
    Ok(cells
        .iter()
        .enumerate()
        .map(|(i, c)| process_cell(c, i, &language))
        .collect())
}

/// Render the processed cells into the model-facing text — the text-block
/// projection of TS `mapNotebookCellsToToolResult` (`notebook.ts:188-215`)
/// after its adjacent-text-block merge.
///
/// Per cell, `cellContentToToolResult` (`:119-132`) emits a single text block:
/// `<cell id="{cell_id}">{metadata}{source}</cell id="{cell_id}">`, where
/// `metadata` is `<cell_type>{cellType}</cell_type>` for a non-code cell and
/// `<language>{language}</language>` for a code cell whose language is not
/// `python`. Each text output then emits a `\n{text}` text block
/// (`cellOutputToToolResult` `:134-153`). The merge reducer joins every
/// adjacent text block with a single `\n` (`:198-213`).
///
/// Image outputs are NOT representable in this single-string model channel
/// (the Rust thin path has no multimodal block list), so only the text blocks
/// are projected — the structured `cells` array (carried in the result `data`)
/// retains the full image payload for the TUI. This is the documented
/// thin-client divergence already used for image/PDF reads.
#[must_use]
pub fn render_cells_model_text(cells: &[Value]) -> String {
    let mut text_blocks: Vec<String> = Vec::new();
    for cell in cells {
        let cell_type = cell.get("cellType").and_then(Value::as_str).unwrap_or("");
        let cell_id = cell.get("cell_id").and_then(Value::as_str).unwrap_or("");
        let source = cell.get("source").and_then(Value::as_str).unwrap_or("");
        let mut metadata = String::new();
        if cell_type != "code" {
            metadata.push_str(&format!("<cell_type>{cell_type}</cell_type>"));
        }
        if cell_type == "code" {
            let language = cell.get("language").and_then(Value::as_str);
            if let Some(lang) = language {
                if lang != "python" {
                    metadata.push_str(&format!("<language>{lang}</language>"));
                }
            }
        }
        text_blocks.push(format!(
            "<cell id=\"{cell_id}\">{metadata}{source}</cell id=\"{cell_id}\">"
        ));
        // Each text output is its own `\n{text}` block.
        if let Some(outputs) = cell.get("outputs").and_then(Value::as_array) {
            for out in outputs {
                if let Some(t) = out.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        text_blocks.push(format!("\n{t}"));
                    }
                }
                // Image outputs become image blocks in TS — omitted from the
                // text projection (see doc comment).
            }
        }
    }
    // Adjacent text blocks are merged with a single `\n` separator (the TS
    // reducer). With only text blocks here, that is just a `\n` join.
    text_blocks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb(cells: Value, language: Option<&str>) -> String {
        let metadata = match language {
            Some(l) => json!({ "language_info": { "name": l } }),
            None => json!({}),
        };
        serde_json::to_string(&json!({
            "cells": cells,
            "metadata": metadata,
            "nbformat": 4,
            "nbformat_minor": 5
        }))
        .unwrap()
    }

    #[test]
    fn code_cell_structured_shape() {
        let raw = nb(
            json!([{
                "cell_type": "code",
                "id": "abc",
                "source": ["print(", "'hi')"],
                "execution_count": 3,
                "outputs": [
                    { "output_type": "stream", "name": "stdout", "text": "hi\n" }
                ]
            }]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        assert_eq!(cells.len(), 1);
        let c = &cells[0];
        assert_eq!(c["cellType"], "code");
        // `source` array joined with no separator.
        assert_eq!(c["source"], "print('hi')");
        assert_eq!(c["execution_count"], 3);
        assert_eq!(c["cell_id"], "abc");
        // default language is python.
        assert_eq!(c["language"], "python");
        assert_eq!(c["outputs"][0]["output_type"], "stream");
        assert_eq!(c["outputs"][0]["text"], "hi\n");
    }

    #[test]
    fn markdown_cell_omits_code_only_fields() {
        let raw = nb(
            json!([{ "cell_type": "markdown", "id": "m1", "source": "# Title" }]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        let c = &cells[0];
        assert_eq!(c["cellType"], "markdown");
        assert_eq!(c["source"], "# Title");
        assert_eq!(c["cell_id"], "m1");
        // No language / execution_count / outputs on a text cell.
        assert!(c.get("language").is_none());
        assert!(c.get("execution_count").is_none());
        assert!(c.get("outputs").is_none());
    }

    #[test]
    fn missing_id_falls_back_to_cell_index() {
        let raw = nb(
            json!([
                { "cell_type": "markdown", "source": "a" },
                { "cell_type": "markdown", "source": "b" }
            ]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        assert_eq!(cells[0]["cell_id"], "cell-0");
        assert_eq!(cells[1]["cell_id"], "cell-1");
    }

    #[test]
    fn null_or_zero_execution_count_is_dropped() {
        let raw = nb(
            json!([
                { "cell_type": "code", "id": "c1", "source": "x", "execution_count": null },
                { "cell_type": "code", "id": "c2", "source": "y", "execution_count": 0 }
            ]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        assert!(cells[0].get("execution_count").is_none());
        assert!(cells[1].get("execution_count").is_none());
    }

    #[test]
    fn language_from_metadata_applied_to_code_cells() {
        let raw = nb(
            json!([{ "cell_type": "code", "id": "c1", "source": "puts 1" }]),
            Some("ruby"),
        );
        let cells = read_notebook(&raw).unwrap();
        assert_eq!(cells[0]["language"], "ruby");
    }

    #[test]
    fn execute_result_extracts_text_and_png_image() {
        let raw = nb(
            json!([{
                "cell_type": "code",
                "id": "c1",
                "source": "df",
                "execution_count": 1,
                "outputs": [{
                    "output_type": "execute_result",
                    "data": {
                        "text/plain": "<table>",
                        "image/png": "AA AA\nBB"
                    },
                    "execution_count": 1
                }]
            }]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        let out = &cells[0]["outputs"][0];
        assert_eq!(out["output_type"], "execute_result");
        assert_eq!(out["text"], "<table>");
        // whitespace stripped from base64 payload.
        assert_eq!(out["image"]["image_data"], "AAAABB");
        assert_eq!(out["image"]["media_type"], "image/png");
    }

    #[test]
    fn error_output_joins_traceback() {
        let raw = nb(
            json!([{
                "cell_type": "code",
                "id": "c1",
                "source": "1/0",
                "execution_count": 1,
                "outputs": [{
                    "output_type": "error",
                    "ename": "ZeroDivisionError",
                    "evalue": "division by zero",
                    "traceback": ["line1", "line2"]
                }]
            }]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        let out = &cells[0]["outputs"][0];
        assert_eq!(out["output_type"], "error");
        assert_eq!(
            out["text"],
            "ZeroDivisionError: division by zero\nline1\nline2"
        );
    }

    #[test]
    fn large_outputs_collapse_to_stub() {
        // A single huge stream output (> LARGE_OUTPUT_THRESHOLD) collapses.
        let big = "x".repeat(LARGE_OUTPUT_THRESHOLD + 1);
        let raw = nb(
            json!([{
                "cell_type": "code",
                "id": "c1",
                "source": "loop",
                "execution_count": 1,
                "outputs": [{ "output_type": "stream", "name": "stdout", "text": big }]
            }]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        let outs = cells[0]["outputs"].as_array().unwrap();
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0]["output_type"], "stream");
        let text = outs[0]["text"].as_str().unwrap();
        assert!(text.starts_with("Outputs are too large to include."));
        assert!(text.contains("jq '.cells[0].outputs'"));
    }

    #[test]
    fn empty_outputs_array_yields_no_outputs_key() {
        let raw = nb(
            json!([{ "cell_type": "code", "id": "c1", "source": "pass", "outputs": [] }]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        assert!(cells[0].get("outputs").is_none());
    }

    #[test]
    fn unknown_output_type_is_dropped() {
        let raw = nb(
            json!([{
                "cell_type": "code",
                "id": "c1",
                "source": "x",
                "execution_count": 1,
                "outputs": [
                    { "output_type": "stream", "text": "ok\n" },
                    { "output_type": "weird_future_type", "blah": 1 }
                ]
            }]),
            None,
        );
        let cells = read_notebook(&raw).unwrap();
        let outs = cells[0]["outputs"].as_array().unwrap();
        // The unknown output is filtered out; only the stream remains.
        assert_eq!(outs.len(), 1);
        assert_eq!(outs[0]["output_type"], "stream");
    }

    #[test]
    fn invalid_json_errors() {
        assert!(read_notebook("not json").is_err());
    }

    #[test]
    fn missing_cells_array_errors() {
        assert!(read_notebook("{\"metadata\":{}}").is_err());
    }

    // ── FT-02: `X2t` per-cell shape gate ─────────────────────────────────────

    #[test]
    fn invalid_notebook_shape_message_is_byte_locked() {
        // `$0i`, cc-238.js @289994340 (238 adds the `source` clause; the 220
        // form `…array of cell objects).` has 0 hits in 238).
        assert_eq!(
            INVALID_NOTEBOOK_SHAPE,
            "Notebook file is not a valid Jupyter notebook (top-level \"cells\" must be an array of cell objects, each with a string or string-array \"source\")."
        );
    }

    #[test]
    fn null_cell_is_refused() {
        // JS leg `r===null`.
        let err = read_notebook(&nb(json!([null]), None)).unwrap_err();
        assert_eq!(err, INVALID_NOTEBOOK_SHAPE);
    }

    #[test]
    fn non_object_cell_is_refused() {
        // JS leg `typeof r!=="object"`.
        for cell in [json!("a string cell"), json!(7), json!(true)] {
            let err = read_notebook(&nb(json!([cell]), None)).unwrap_err();
            assert_eq!(err, INVALID_NOTEBOOK_SHAPE);
        }
    }

    #[test]
    fn cell_without_source_is_refused() {
        // JS leg `typeof r.source!=="string" && !(Array.isArray(...))` with
        // `r.source === undefined`. Before FT-02 the port accepted this and
        // rendered `source: null`.
        let err =
            read_notebook(&nb(json!([{ "cell_type": "code", "id": "c1" }]), None)).unwrap_err();
        assert_eq!(err, INVALID_NOTEBOOK_SHAPE);
    }

    #[test]
    fn cell_with_non_string_source_is_refused() {
        for src in [json!(42), json!(null), json!({ "a": 1 })] {
            let err = read_notebook(&nb(json!([{ "cell_type": "code", "source": src }]), None))
                .unwrap_err();
            assert_eq!(err, INVALID_NOTEBOOK_SHAPE);
        }
    }

    #[test]
    fn cell_with_mixed_source_array_is_refused() {
        // `r.source.every((n)=>typeof n==="string")` fails on the `1`.
        let err = read_notebook(&nb(
            json!([{ "cell_type": "code", "source": ["ok", 1] }]),
            None,
        ))
        .unwrap_err();
        assert_eq!(err, INVALID_NOTEBOOK_SHAPE);
    }

    #[test]
    fn empty_source_array_is_accepted() {
        // JS `[].every(...)` is `true`, so an empty array is a VALID source —
        // the gate must not over-reject.
        let cells = read_notebook(&nb(
            json!([{ "cell_type": "code", "id": "c1", "source": [] }]),
            None,
        ))
        .unwrap();
        assert_eq!(cells[0]["source"], "");
    }

    #[test]
    fn one_bad_cell_refuses_the_whole_notebook() {
        // `!t.some(...)` — the gate is all-or-nothing, not per-cell filtering.
        let err = read_notebook(&nb(
            json!([
                { "cell_type": "code", "id": "c1", "source": "ok" },
                { "cell_type": "code", "id": "c2" }
            ]),
            None,
        ))
        .unwrap_err();
        assert_eq!(err, INVALID_NOTEBOOK_SHAPE);
    }

    #[test]
    fn is_valid_notebook_shape_mirrors_x2t() {
        assert!(is_valid_notebook_shape(
            &json!({ "cells": [{ "source": "x" }] })
        ));
        assert!(is_valid_notebook_shape(&json!({ "cells": [] })));
        assert!(!is_valid_notebook_shape(&json!({ "cells": {} })));
        assert!(!is_valid_notebook_shape(&json!({})));
        assert!(!is_valid_notebook_shape(&json!({ "cells": [{}] })));
    }

    // ── FT-04: the 100 MiB cap + non-regular-file guard ──────────────────────

    #[test]
    fn notebook_size_cap_and_guards_are_byte_locked() {
        // `var aV=104857600` and `K0f`'s first throw (cc-238.js @289994340).
        assert_eq!(MAX_NOTEBOOK_READ_SIZE, 104_857_600);
        assert_eq!(
            NOTEBOOK_NOT_REGULAR_FILE,
            "Notebook path is not a regular file (device, FIFO, or socket)."
        );
        // `l8n()` — `Ba(104857600)` renders `100MB`.
        assert_eq!(
            notebook_too_large_message(),
            "Notebook file exceeds the maximum size this tool can read (100MB). Use a registered shell tool with jq to read specific portions:\n  cat <notebook_path> | jq '.cells[:20]' # First 20 cells\n  cat <notebook_path> | jq '.cells[100:120]' # Cells 100-120\n  cat <notebook_path> | jq '.cells | length' # Count total cells\n  cat <notebook_path> | jq '.cells[] | select(.cell_type==\"code\") | .source' # All code sources"
        );
    }

    #[test]
    fn format_output_text_truncates_with_byte_locked_suffix() {
        let body = format!("{}\ntail1\ntail2\n", "a".repeat(NOTEBOOK_OUTPUT_MAX_LENGTH));
        let out = format_output_text(&body);
        assert!(out.contains("\n\n... ["));
        assert!(out.contains("lines truncated] ..."));
    }

    #[test]
    fn render_model_text_code_cell_no_metadata() {
        // A python code cell: no <cell_type>, no <language> (python suppressed).
        let cells = read_notebook(&nb(
            json!([{ "cell_type": "code", "id": "c1", "source": "print(1)" }]),
            None,
        ))
        .unwrap();
        assert_eq!(
            render_cells_model_text(&cells),
            "<cell id=\"c1\">print(1)</cell id=\"c1\">"
        );
    }

    #[test]
    fn render_model_text_markdown_cell_has_cell_type() {
        let cells = read_notebook(&nb(
            json!([{ "cell_type": "markdown", "id": "m1", "source": "# Hi" }]),
            None,
        ))
        .unwrap();
        assert_eq!(
            render_cells_model_text(&cells),
            "<cell id=\"m1\"><cell_type>markdown</cell_type># Hi</cell id=\"m1\">"
        );
    }

    #[test]
    fn render_model_text_non_python_code_cell_has_language() {
        let cells = read_notebook(&nb(
            json!([{ "cell_type": "code", "id": "c1", "source": "puts 1" }]),
            Some("ruby"),
        ))
        .unwrap();
        assert_eq!(
            render_cells_model_text(&cells),
            "<cell id=\"c1\"><language>ruby</language>puts 1</cell id=\"c1\">"
        );
    }

    #[test]
    fn render_model_text_appends_output_text() {
        let cells = read_notebook(&nb(
            json!([{
                "cell_type": "code",
                "id": "c1",
                "source": "print('hi')",
                "execution_count": 1,
                "outputs": [{ "output_type": "stream", "name": "stdout", "text": "hi\n" }]
            }]),
            None,
        ))
        .unwrap();
        // The cell block, then a `\n{text}` output block, joined by the merge `\n`.
        assert_eq!(
            render_cells_model_text(&cells),
            "<cell id=\"c1\">print('hi')</cell id=\"c1\">\n\nhi\n"
        );
    }

    #[test]
    fn render_model_text_joins_multiple_cells() {
        let cells = read_notebook(&nb(
            json!([
                { "cell_type": "code", "id": "c1", "source": "a" },
                { "cell_type": "markdown", "id": "c2", "source": "b" }
            ]),
            None,
        ))
        .unwrap();
        assert_eq!(
            render_cells_model_text(&cells),
            "<cell id=\"c1\">a</cell id=\"c1\">\n<cell id=\"c2\"><cell_type>markdown</cell_type>b</cell id=\"c2\">"
        );
    }
}
