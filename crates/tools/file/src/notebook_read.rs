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

use crate::Utf16JsonProjection;
use lingxi_core::types::utf16_json::JsonValueView;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

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

fn exact_string(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display text comes from the same source units")
}

fn source_value(view: JsonValueView<'_>) -> Utf16JsonProjection {
    view.to_projection()
        .expect("a parsed JSON view is a valid source")
}

/// Native String.slice counts UTF-16 units and can end on an unpaired unit.
/// Count omitted newlines from that same unit boundary before adding the hint.
fn format_output_units(mut content: Vec<u16>) -> Vec<u16> {
    if content.len() <= NOTEBOOK_OUTPUT_MAX_LENGTH {
        return content;
    }
    let remaining_lines = content[NOTEBOOK_OUTPUT_MAX_LENGTH..]
        .iter()
        .filter(|unit| **unit == 10)
        .count()
        + 1;
    content.truncate(NOTEBOOK_OUTPUT_MAX_LENGTH);
    content.extend(format!("\n\n... [{remaining_lines} lines truncated] ...").encode_utf16());
    content
}

#[cfg(test)]
fn format_output_text(content: &str) -> String {
    String::from_utf16_lossy(&format_output_units(content.encode_utf16().collect()))
}

fn joined_text_units(text: Option<JsonValueView<'_>>) -> Vec<u16> {
    let Some(text) = text else {
        return Vec::new();
    };
    if let Some(units) = text.code_units() {
        return units.to_vec();
    }
    let mut units = Vec::new();
    for index in 0..text.array_len().unwrap_or(0) {
        if let Some(part) = text.get_index(index).and_then(JsonValueView::code_units) {
            units.extend_from_slice(part);
        }
    }
    units
}

fn process_output_text_source(text: Option<JsonValueView<'_>>) -> Utf16JsonProjection {
    exact_string(format_output_units(joined_text_units(text)))
}

fn extract_image_source(data: JsonValueView<'_>) -> Option<Utf16JsonProjection> {
    for media_type in ["image/png", "image/jpeg"] {
        if let Some(units) = data
            .get_property(media_type)
            .and_then(JsonValueView::code_units)
        {
            let units = units
                .iter()
                .copied()
                .filter(|unit| {
                    !char::from_u32(u32::from(*unit))
                        .is_some_and(lingxi_core::host::effort::javascript_whitespace)
                })
                .collect();
            return Some(
                Utf16JsonProjection::object(vec![
                    ("image_data".into(), exact_string(units)),
                    ("media_type".into(), media_type.into()),
                ])
                .expect("notebook image output"),
            );
        }
    }
    None
}

fn output_size_source(output: &Utf16JsonProjection) -> usize {
    output
        .at_pointer("/text")
        .and_then(JsonValueView::code_units)
        .map_or(0, <[u16]>::len)
        .saturating_add(
            output
                .at_pointer("/image/image_data")
                .and_then(JsonValueView::code_units)
                .map_or(0, <[u16]>::len),
        )
}

fn process_output_source(output: JsonValueView<'_>) -> Option<Utf16JsonProjection> {
    let output_type = output
        .get_property("output_type")
        .and_then(JsonValueView::as_str)?;
    let mut fields = vec![("output_type".into(), output_type.into())];
    match output_type {
        "stream" => fields.push((
            "text".into(),
            process_output_text_source(output.get_property("text")),
        )),
        "execute_result" | "display_data" => {
            let data = output.get_property("data");
            fields.push((
                "text".into(),
                process_output_text_source(data.and_then(|data| data.get_property("text/plain"))),
            ));
            if let Some(image) = data.and_then(extract_image_source) {
                fields.push(("image".into(), image));
            }
        }
        "error" => {
            let mut combined = output
                .get_property("ename")
                .and_then(JsonValueView::code_units)
                .unwrap_or_default()
                .to_vec();
            combined.extend(": ".encode_utf16());
            combined.extend_from_slice(
                output
                    .get_property("evalue")
                    .and_then(JsonValueView::code_units)
                    .unwrap_or_default(),
            );
            combined.push(10);
            if let Some(traceback) = output.get_property("traceback") {
                let mut has_line = false;
                for index in 0..traceback.array_len().unwrap_or(0) {
                    if let Some(line) = traceback
                        .get_index(index)
                        .and_then(JsonValueView::code_units)
                    {
                        if has_line {
                            combined.push(10);
                        }
                        combined.extend_from_slice(line);
                        has_line = true;
                    }
                }
            }
            fields.push(("text".into(), exact_string(format_output_units(combined))));
        }
        _ => return None,
    }
    Some(Utf16JsonProjection::object(fields).expect("processed notebook output"))
}

fn valid_cell_source(cell: JsonValueView<'_>) -> bool {
    if cell.object_entries().is_none() {
        return false;
    }
    let Some(source) = cell.get_property("source") else {
        return false;
    };
    source.code_units().is_some()
        || source.array_len().is_some_and(|count| {
            (0..count).all(|index| {
                source
                    .get_index(index)
                    .and_then(JsonValueView::code_units)
                    .is_some()
            })
        })
}

fn process_cell_source(
    cell: JsonValueView<'_>,
    index: usize,
    language: &Utf16JsonProjection,
) -> Utf16JsonProjection {
    let cell_type = cell
        .get_property("cell_type")
        .filter(|value| value.code_units().is_some());
    let is_code = cell_type.and_then(JsonValueView::as_str) == Some("code");
    let mut fields = vec![
        (
            "cellType".into(),
            cell_type.map_or_else(|| "".into(), source_value),
        ),
        (
            "source".into(),
            exact_string(joined_text_units(cell.get_property("source"))),
        ),
    ];
    if is_code {
        if let Some(count) = cell
            .get_property("execution_count")
            .filter(|count| count.as_number().is_some_and(|value| value != 0.0))
        {
            fields.push(("execution_count".into(), source_value(count)));
        }
    }
    let id = cell
        .get_property("id")
        .filter(|value| value.code_units().is_some())
        .map_or_else(|| format!("cell-{index}").into(), source_value);
    fields.push(("cell_id".into(), id));
    if is_code {
        fields.push(("language".into(), language.clone()));
        if let Some(outputs) = cell
            .get_property("outputs")
            .filter(|value| value.array_len().is_some_and(|count| count > 0))
        {
            let mut processed = Vec::new();
            let mut total = 0usize;
            for index in 0..outputs.array_len().unwrap_or(0) {
                if let Some(output) = outputs.get_index(index).and_then(process_output_source) {
                    total = total.saturating_add(output_size_source(&output));
                    processed.push(output);
                }
            }
            if total > LARGE_OUTPUT_THRESHOLD {
                processed = vec![Utf16JsonProjection::object(vec![
                    ("output_type".into(), "stream".into()),
                    ("text".into(), format!("Outputs are too large to include. Use {BASH_TOOL_NAME} with: cat <notebook_path> | jq '.cells[{index}].outputs'").into()),
                ]).expect("large output stub")];
            }
            fields.push((
                "outputs".into(),
                Utf16JsonProjection::array(processed).expect("processed notebook outputs"),
            ));
        }
    }
    Utf16JsonProjection::object(fields).expect("processed notebook cell")
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
    // Compatibility display view. Production Read uses the exact source API.
    let source = read_notebook_source(raw)?;
    let Value::Array(mut cells) = source
        .try_to_serde_value_with_array_budget(usize::MAX)
        .map_err(|error| error.to_string())?
    else {
        unreachable!("processed cells are an array");
    };
    // Keep the compatibility view's integer representation without sending
    // source strings through serde. The only numeric projected field is count.
    for (index, cell) in cells.iter_mut().enumerate() {
        if let Some(count) = cell.get_mut("execution_count") {
            let number = source
                .at_pointer(&format!("/{index}/execution_count"))
                .expect("processed execution count")
                .to_projection()
                .map_err(|error| error.to_string())?
                .to_json_string()
                .map_err(|error| error.to_string())?;
            *count = serde_json::from_str(&number).map_err(|error| error.to_string())?;
        }
    }
    Ok(cells)
}

/// Parse ECMAScript JSON and project notebook cells without a serde string
/// boundary. Literal and escaped lone surrogate units survive every mapped
/// string; unknown source metadata stays in the parsed carrier until projection.
pub fn read_notebook_source(raw: &str) -> Result<Utf16JsonProjection, String> {
    let notebook = Utf16JsonProjection::parse(raw).map_err(|error| {
        // Preserve prior malformed-syntax diagnostics. Valid ECMAScript JSON,
        // including lone surrogate escapes, never enters this fallback.
        let diagnostic = serde_json::from_str::<Value>(raw).err()
            .map_or_else(|| error.to_string(), |error| error.to_string());
        format!("Notebook file is not valid JSON (it may be truncated, corrupted, or still being written): {diagnostic}")
    })?;
    let cells = notebook
        .at_pointer("/cells")
        .filter(|cells| cells.array_len().is_some())
        .ok_or_else(|| INVALID_NOTEBOOK_SHAPE.to_string())?;
    let count = cells.array_len().expect("checked cell array");
    if !(0..count).all(|index| cells.get_index(index).is_some_and(valid_cell_source)) {
        return Err(INVALID_NOTEBOOK_SHAPE.to_string());
    }
    let language = notebook
        .at_pointer("/metadata/language_info/name")
        .filter(|value| value.code_units().is_some())
        .map_or_else(|| "python".into(), source_value);
    let processed = (0..count)
        .map(|index| {
            process_cell_source(
                cells.get_index(index).expect("validated notebook cell"),
                index,
                &language,
            )
        })
        .collect();
    Utf16JsonProjection::array(processed).map_err(|error| error.to_string())
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

/// Source-preserving equivalent used when a Mod replaces a Read result.
pub(crate) fn render_cells_model_text_source(
    cells: &crate::Utf16JsonProjection,
) -> Option<crate::Utf16JsonProjection> {
    let count = cells.array_len("")?;
    let code = "code".encode_utf16().collect::<Vec<_>>();
    let python = "python".encode_utf16().collect::<Vec<_>>();
    let mut text_blocks: Vec<Vec<u16>> = Vec::new();
    for index in 0..count {
        let pointer = format!("/{index}");
        let cell_type = cells
            .string_units(&format!("{pointer}/cellType"))
            .unwrap_or_default();
        let cell_id = cells
            .string_units(&format!("{pointer}/cell_id"))
            .unwrap_or_default();
        let source = cells
            .string_units(&format!("{pointer}/source"))
            .unwrap_or_default();
        let mut block = Vec::new();
        block.extend("<cell id=\"".encode_utf16());
        block.extend_from_slice(&cell_id);
        block.extend("\">".encode_utf16());
        if cell_type != code {
            block.extend("<cell_type>".encode_utf16());
            block.extend_from_slice(&cell_type);
            block.extend("</cell_type>".encode_utf16());
        }
        if cell_type == code {
            if let Some(language) = cells.string_units(&format!("{pointer}/language")) {
                if language != python {
                    block.extend("<language>".encode_utf16());
                    block.extend_from_slice(&language);
                    block.extend("</language>".encode_utf16());
                }
            }
        }
        block.extend_from_slice(&source);
        block.extend("</cell id=\"".encode_utf16());
        block.extend_from_slice(&cell_id);
        block.extend("\">".encode_utf16());
        text_blocks.push(block);

        let outputs = format!("{pointer}/outputs");
        if let Some(output_count) = cells.array_len(&outputs) {
            for output_index in 0..output_count {
                if let Some(text) = cells.string_units(&format!("{outputs}/{output_index}/text")) {
                    if !text.is_empty() {
                        let mut output_block = vec![b'\n' as u16];
                        output_block.extend(text);
                        text_blocks.push(output_block);
                    }
                }
            }
        }
    }
    let total_units =
        text_blocks.iter().map(Vec::len).sum::<usize>() + text_blocks.len().saturating_sub(1);
    let mut units = Vec::with_capacity(total_units);
    for (index, block) in text_blocks.iter().enumerate() {
        if index > 0 {
            units.push(b'\n' as u16);
        }
        units.extend_from_slice(block);
    }
    let display = String::from_utf16_lossy(&units);
    Some(
        crate::Utf16JsonProjection::root_string(display, units)
            .expect("notebook display mirrors its UTF-16 source units"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_notebook_read_retains_exact_ids_source_language_and_output_strings() {
        let raw = r#"{"cells":[{"cell_type":"code","id":"c\ud800","source":["a\ud801","x\udc01"],"execution_count":1,"outputs":[{"output_type":"stream","text":["o\ud802","z\udc02"]},{"output_type":"display_data","data":{"text/plain":"d\ud803","image/png":"AA AA\nBB"}},{"output_type":"error","ename":"E\ud804","evalue":"v\udc04","traceback":["t\ud805","u\udc05"]}]}],"metadata":{"language_info":{"name":"julia\ud806"},"unknown\ud807":{"value":"keep\udc07"}}}"#;
        let cells = read_notebook_source(raw).unwrap();
        assert_eq!(cells.string_units("/0/cell_id"), Some(vec![99, 0xd800]));
        assert_eq!(
            cells.string_units("/0/source"),
            Some(vec![97, 0xd801, 120, 0xdc01])
        );
        assert_eq!(
            cells.string_units("/0/language"),
            Some("julia".encode_utf16().chain([0xd806]).collect())
        );
        assert_eq!(
            cells.string_units("/0/outputs/0/text"),
            Some(vec![111, 0xd802, 122, 0xdc02])
        );
        assert_eq!(
            cells.string_units("/0/outputs/1/text"),
            Some(vec![100, 0xd803])
        );
        assert_eq!(
            cells.string_at("/0/outputs/1/image/image_data"),
            Some("AAAABB")
        );
        assert_eq!(
            cells.string_units("/0/outputs/2/text"),
            Some(vec![
                69, 0xd804, 58, 32, 118, 0xdc04, 10, 116, 0xd805, 10, 117, 0xdc05
            ])
        );
        let model = render_cells_model_text_source(&cells)
            .unwrap()
            .string_units("")
            .unwrap();
        for unit in [
            0xd800, 0xd801, 0xdc01, 0xd802, 0xdc02, 0xd803, 0xd804, 0xdc04, 0xd805, 0xdc05, 0xd806,
        ] {
            assert!(model.contains(&unit), "model output lost {unit:04x}");
        }
        let serialized = cells.to_json_string().unwrap();
        let reparsed = Utf16JsonProjection::parse(&serialized).unwrap();
        assert_eq!(
            cells.string_units("/0/source"),
            reparsed.string_units("/0/source")
        );
    }

    #[test]
    fn source_notebook_output_limits_count_native_utf16_units() {
        let raw = nb(
            json!([{"cell_type":"code","source":"x","outputs":[{"output_type":"stream","text":"中".repeat(6000)}]}]),
            None,
        );
        let cells = read_notebook_source(&raw).unwrap();
        assert_eq!(cells.string_units("/0/outputs/0/text").unwrap().len(), 6000);
        let raw = nb(
            json!([{"cell_type":"code","source":"x","outputs":[{"output_type":"stream","text":"中".repeat(10001)}]}]),
            None,
        );
        assert!(read_notebook_source(&raw)
            .unwrap()
            .string_at("/0/outputs/0/text")
            .unwrap()
            .starts_with("Outputs are too large"));
        let mut units = vec![0xd800; NOTEBOOK_OUTPUT_MAX_LENGTH + 2];
        units.push(10);
        let formatted = format_output_units(units);
        assert_eq!(
            &formatted[..NOTEBOOK_OUTPUT_MAX_LENGTH],
            vec![0xd800; NOTEBOOK_OUTPUT_MAX_LENGTH].as_slice()
        );
        assert_eq!(
            String::from_utf16_lossy(&formatted[NOTEBOOK_OUTPUT_MAX_LENGTH..]),
            "\n\n... [2 lines truncated] ..."
        );
    }

    #[test]
    fn source_notebook_read_rejects_malformed_json_and_source_shapes() {
        for raw in [
            r#"{"cells":[{"source":"\ud800"}]"#,
            r#"{"cells":[{"source":"\uD80"}]}"#,
        ] {
            assert!(read_notebook_source(raw)
                .unwrap_err()
                .starts_with("Notebook file is not valid JSON"));
        }
        for raw in [
            r#"{"cells":[{"source":["\ud800",1]}]}"#,
            r#"{"cells":[{"metadata":{"unknown":"\ud800"}}]}"#,
        ] {
            assert_eq!(
                read_notebook_source(raw).unwrap_err(),
                INVALID_NOTEBOOK_SHAPE
            );
        }
    }

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
    fn source_model_text_keeps_lone_units_from_cell_and_output_strings() {
        let cells = crate::Utf16JsonProjection::parse(
            r#"[{"cellType":"code","cell_id":"c\ud800","source":"a\ud800b","outputs":[{"text":"o\ud800"}]}]"#,
        )
        .unwrap();
        let rendered = render_cells_model_text_source(&cells).expect("array source renders");
        let mut expected = "<cell id=\"c".encode_utf16().collect::<Vec<_>>();
        expected.push(0xd800);
        expected.extend("\">a".encode_utf16());
        expected.push(0xd800);
        expected.extend("b</cell id=\"c".encode_utf16());
        expected.push(0xd800);
        expected.extend("\">\n\no".encode_utf16());
        expected.push(0xd800);
        assert_eq!(rendered.string_units(""), Some(expected));
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
