//! The ONE derivation of "does this tool result carry something the model must
//! see as media, rather than as text".
//!
//! It lives here because two call sites need it and they are in different
//! crates: the main turn loop (`orchestrator::turn_loop`) and the subagent
//! runner (`agent::runner`). Both depend on `tool-api`, and a second copy in
//! either of them would be a second derivation of the same rule — the shape
//! that always diverges, and diverges silently, because a tool result that
//! quietly degrades to text still LOOKS like a successful call.
//!
//! That is not hypothetical. Before this module existed only the main loop
//! derived image blocks; the subagent runner hardcoded `content_blocks: None`.
//! `LocalAppCaptureUi` therefore worked in a normal conversation and returned
//! ~230 KB of base64 as plain TEXT to the `frontend-qa` verify subagent, which
//! is the only caller the capture tool exists for. The subagent then reported
//! `render_check.status = "passed"` — honestly, from metadata — for a frame it
//! had never been able to look at.

use crate::tool_trait::{source_text_projection, source_text_projection_units};
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};

/// Tool-aware media projection shared by the main and child loops.
#[must_use]
pub fn media_content_blocks_for_tool(
    name: &str,
    data: &Utf16JsonProjection,
    model_text: &Utf16JsonProjection,
) -> Option<Vec<Utf16JsonProjection>> {
    match name {
        "Bash" => bash_image_content_blocks(data).or_else(|| media_content_blocks(data)),
        "Read" => {
            read_media_content_blocks(data, model_text).or_else(|| media_content_blocks(data))
        }
        "computer" => {
            computer_batch_content_blocks(data, model_text).or_else(|| media_content_blocks(data))
        }
        _ => media_content_blocks(data),
    }
}

fn computer_batch_images(
    data: &Utf16JsonProjection,
) -> Option<Vec<(usize, Vec<Utf16JsonProjection>)>> {
    let steps_completed = data
        .get_property("stepsCompleted")
        .and_then(JsonValueView::as_number)?;
    if !steps_completed.is_finite()
        || steps_completed < 0.0
        || steps_completed.fract() != 0.0
        || steps_completed >= u64::MAX as f64
    {
        return None;
    }
    let results = data.get_property("results")?;
    let images: Vec<_> = (0..results.array_len()?)
        .filter_map(|index| {
            let item = results.get_index(index)?;
            let action = item
                .get_property("action")
                .and_then(JsonValueView::as_str)?;
            if !matches!(action, "screenshot" | "zoom") {
                return None;
            }
            image_content_blocks(
                &data
                    .subprojection(&format!("/results/{index}/result"))
                    .ok()?,
            )
            .map(|blocks| (index, blocks))
        })
        .collect();
    (!images.is_empty()).then_some(images)
}

/// Return the Computer batch summary with image bytes removed, serialized as
/// the ordinary model-facing text string. All untouched fields stay in the
/// exact UTF-16 projection while the image payload is omitted.
#[must_use]
pub fn computer_batch_model_text(data: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let images = computer_batch_images(data)?;
    let mut results = data.subprojection("/results").ok()?;
    for (index, _) in images {
        let mut file = data
            .subprojection(&format!("/results/{index}/result/file"))
            .ok()?;
        file.remove_field("base64").ok()?;
        let mut result = data
            .subprojection(&format!("/results/{index}/result"))
            .ok()?;
        result.set_field("file", file).ok()?;
        let mut item = data.subprojection(&format!("/results/{index}")).ok()?;
        item.set_field("result", result).ok()?;
        results
            .replace_pointer_projection(&format!("/{index}"), item)
            .ok()?;
    }
    let mut metadata = data.clone();
    metadata.set_field("results", results).ok()?;
    Some(source_text_projection(&metadata.to_json_string().ok()?))
}

fn computer_batch_content_blocks(
    data: &Utf16JsonProjection,
    model_text: &Utf16JsonProjection,
) -> Option<Vec<Utf16JsonProjection>> {
    let images = computer_batch_images(data)?;
    let results = data.get_property("results")?;
    let mut blocks = vec![text_block(model_text.clone())];
    for (index, image) in images {
        let action = results
            .get_index(index)?
            .get_property("action")?
            .code_units()?;
        let mut label: Vec<u16> = format!("[Step {}: ", index + 1).encode_utf16().collect();
        label.extend_from_slice(action);
        label.push(u16::from(b']'));
        blocks.push(text_block(source_text_projection_units(&label)));
        blocks.extend(image);
    }
    Some(blocks)
}

/// Read's notebook, PDF, and extracted-page result mapper. The summary must be
/// the same tool-owned text used for an ordinary result; blocks replace it on
/// the wire.
#[must_use]
pub fn read_media_content_blocks(
    data: &Utf16JsonProjection,
    model_text: &Utf16JsonProjection,
) -> Option<Vec<Utf16JsonProjection>> {
    let file = data.get_property("file")?;
    let kind = data.get_property("type")?.code_units()?;
    let kind_notebook: Vec<u16> = "notebook".encode_utf16().collect();
    let kind_pdf: Vec<u16> = "pdf".encode_utf16().collect();
    let kind_parts: Vec<u16> = "parts".encode_utf16().collect();
    if kind == kind_notebook {
        let cells = file.get_property("cells")?;
        let cell_count = cells.array_len()?;
        let mut blocks: Vec<Utf16JsonProjection> = Vec::new();
        for index in 0..cell_count {
            let cell = cells.get_index(index)?;
            let cell_type = cell.get_property("cellType")?.code_units()?;
            let cell_id = cell.get_property("cell_id")?.code_units()?;
            let source = cell.get_property("source")?.code_units()?;
            let mut text_units = Vec::new();
            if cell_type != "code".encode_utf16().collect::<Vec<_>>() {
                append_ascii(&mut text_units, "<cell_type>");
                text_units.extend_from_slice(cell_type);
                append_ascii(&mut text_units, "</cell_type>");
            } else if let Some(language) = cell
                .get_property("language")
                .and_then(JsonValueView::code_units)
            {
                if language != "python".encode_utf16().collect::<Vec<_>>() {
                    append_ascii(&mut text_units, "<language>");
                    text_units.extend_from_slice(language);
                    append_ascii(&mut text_units, "</language>");
                }
            }
            let mut cell_text = Vec::new();
            append_ascii(&mut cell_text, "<cell id=\"");
            cell_text.extend_from_slice(cell_id);
            append_ascii(&mut cell_text, "\">");
            cell_text.extend(text_units);
            cell_text.extend_from_slice(source);
            append_ascii(&mut cell_text, "</cell id=\"");
            cell_text.extend_from_slice(cell_id);
            append_ascii(&mut cell_text, "\">");
            append_read_text_block(&mut blocks, cell_text);

            if let Some(outputs) = cell.get_property("outputs") {
                if let Some(output_count) = outputs.array_len() {
                    for output_index in 0..output_count {
                        let output = outputs.get_index(output_index)?;
                        if let Some(text) = output
                            .get_property("text")
                            .and_then(JsonValueView::code_units)
                        {
                            if !text.is_empty() {
                                let mut line = vec![u16::from(b'\n')];
                                line.extend_from_slice(text);
                                append_read_text_block(&mut blocks, line);
                            }
                        }
                        if let Some(image) = output.get_property("image") {
                            let base64 = image.get_property("image_data")?.code_units()?;
                            let media_type = image.get_property("media_type")?.code_units()?;
                            blocks.push(media_block(
                                "image",
                                source_text_projection_units(base64),
                                source_text_projection_units(media_type),
                            ));
                        }
                    }
                }
            }
        }
        return Some(blocks);
    }
    if kind == kind_pdf {
        let base64 = file.get_property("base64")?.code_units()?;
        if base64.is_empty() {
            return None;
        }
        return Some(vec![
            text_block(model_text.clone()),
            media_block(
                "document",
                source_text_projection_units(base64),
                source_text_projection("application/pdf"),
            ),
        ]);
    }
    if kind == kind_parts {
        let pages = data.get_property("pages")?;
        let page_count = pages.array_len()?;
        if page_count == 0 {
            return None;
        }
        let first_page = data
            .get_property("firstPage")
            .and_then(JsonValueView::as_number)
            .filter(|number| *number >= 0.0 && number.fract() == 0.0)
            .map_or(1, |number| number as u64);
        let mut blocks = vec![text_block(model_text.clone())];
        for index in 0..page_count {
            let page = pages.get_index(index)?;
            let base64 = page.get_property("base64")?.code_units()?;
            if !base64.is_empty() {
                let media_type = page.get_property("mediaType")?.code_units()?;
                blocks.push(media_block(
                    "image",
                    source_text_projection_units(base64),
                    source_text_projection_units(media_type),
                ));
            } else {
                let mut suffix = Vec::new();
                if let Some(error) = page
                    .get_property("error")
                    .and_then(JsonValueView::code_units)
                    .filter(|error| !error.is_empty())
                {
                    append_ascii(&mut suffix, ": ");
                    suffix.extend_from_slice(error);
                }
                let text = format!(
                    "[Page {} could not be processed as an image",
                    first_page.saturating_add(index as u64)
                );
                let mut text_units: Vec<u16> = text.encode_utf16().collect();
                text_units.extend(suffix);
                text_units.push(u16::from(b']'));
                blocks.push(text_block(source_text_projection_units(&text_units)));
            }
        }
        return Some(blocks);
    }
    None
}

fn append_ascii(output: &mut Vec<u16>, text: &str) {
    output.extend(text.encode_utf16());
}

fn append_read_text_block(blocks: &mut Vec<Utf16JsonProjection>, text: Vec<u16>) {
    if let Some(last) = blocks.last_mut() {
        if let Some(previous) = last
            .get_property("text")
            .and_then(JsonValueView::code_units)
        {
            let mut merged = previous.to_vec();
            merged.push(u16::from(b'\n'));
            merged.extend(text);
            *last = text_block(source_text_projection_units(&merged));
            return;
        }
    }
    blocks.push(text_block(source_text_projection_units(&text)));
}

fn source_object(fields: Vec<(&str, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
    .expect("locally assembled media block is a JSON object")
}

fn text_block(text: Utf16JsonProjection) -> Utf16JsonProjection {
    source_object(vec![
        ("type", source_text_projection("text")),
        ("text", text),
    ])
}

fn media_block(
    kind: &str,
    base64: Utf16JsonProjection,
    media_type: Utf16JsonProjection,
) -> Utf16JsonProjection {
    let source = source_object(vec![
        ("type", source_text_projection("base64")),
        ("data", base64),
        ("media_type", media_type),
    ]);
    source_object(vec![
        ("type", source_text_projection(kind)),
        ("source", source),
    ])
}

/// Claude's Bash `isImage` mapper: require a base64 data URI and sniff the
/// decoded payload instead of trusting the URI's claimed media type.
#[must_use]
pub fn bash_image_content_blocks(data: &Utf16JsonProjection) -> Option<Vec<Utf16JsonProjection>> {
    use base64::Engine as _;
    if data
        .get_property("isImage")
        .and_then(JsonValueView::as_bool)
        != Some(true)
    {
        return None;
    }
    let stdout = data.get_property("stdout")?.code_units()?;
    let stdout = String::from_utf16(stdout).ok()?;
    let rest = stdout.trim().strip_prefix("data:")?;
    let semi = rest.find(';')?;
    if semi == 0 {
        return None;
    }
    let payload = rest[semi..].strip_prefix(";base64,")?;
    if payload.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let media_type = crate::util::image_sniff::sniff_image_media_type(&bytes)?;
    Some(vec![media_block(
        "image",
        source_text_projection(payload),
        source_text_projection(media_type),
    )])
}

/// The media blocks a tool result's `data` should be delivered as, or `None`
/// when it is an ordinary textual result.
#[must_use]
pub fn media_content_blocks(data: &Utf16JsonProjection) -> Option<Vec<Utf16JsonProjection>> {
    let root = data.at_pointer("")?;
    if let Some(count) = root.array_len() {
        if count == 0 {
            return None;
        }
        return (0..count)
            .map(|index| data.subprojection(&format!("/{index}")).ok())
            .collect();
    }
    image_content_blocks(data)
}

/// The `{type:"image", file:{base64, type}}` shape.
#[must_use]
pub fn image_content_blocks(data: &Utf16JsonProjection) -> Option<Vec<Utf16JsonProjection>> {
    if data.get_property("type").and_then(JsonValueView::as_str) != Some("image") {
        return None;
    }
    let file = data.get_property("file")?;
    let base64 = file.get_property("base64")?.code_units()?;
    if base64.is_empty() {
        return None;
    }
    let media_type = file.get_property("type")?.code_units()?;
    Some(vec![media_block(
        "image",
        source_text_projection_units(base64),
        source_text_projection_units(media_type),
    )])
}

/// Source summary for a result whose base64/data payload should stay ephemeral.
#[must_use]
pub fn ephemeral_summary(data: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    if data
        .get_property("_lingxi_ephemeral")
        .and_then(JsonValueView::as_bool)
        != Some(true)
    {
        return None;
    }
    let summary = data.get_property("summary")?;
    summary.code_units()?;
    data.subprojection("/summary").ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_trait::generated_builtin_result;
    use serde_json::json;

    fn source_json_fixture(value: serde_json::Value) -> Utf16JsonProjection {
        generated_builtin_result(&value)
    }

    #[test]
    fn a_captured_frame_becomes_an_image_block() {
        let data = source_json_fixture(json!({
            "type": "image",
            "file": { "base64": "AAAA", "type": "image/jpeg" },
        }));
        let blocks = media_content_blocks(&data).expect("an image result carries blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].string_at("/type"), Some("image"));
        assert_eq!(blocks[0].string_at("/source/data"), Some("AAAA"));
        assert_eq!(
            blocks[0].string_at("/source/media_type"),
            Some("image/jpeg")
        );
    }

    #[test]
    fn an_empty_payload_is_not_an_image() {
        let data = source_json_fixture(json!({
            "type": "image",
            "file": { "base64": "", "type": "image/jpeg" },
        }));
        assert!(media_content_blocks(&data).is_none());
    }

    #[test]
    fn a_missing_media_type_is_not_an_image() {
        let data = source_json_fixture(json!({ "type": "image", "file": { "base64": "AAAA" } }));
        assert!(media_content_blocks(&data).is_none());
    }

    #[test]
    fn an_mcp_array_passes_through_and_an_empty_one_does_not() {
        let data = source_json_fixture(json!([{ "type": "text", "text": "hi" }]));
        let blocks = media_content_blocks(&data).expect("array passes through");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].string_at("/text"), Some("hi"));
        assert!(media_content_blocks(&source_json_fixture(json!([]))).is_none());
    }

    #[test]
    fn ordinary_results_stay_text() {
        for value in [
            json!({ "ok": true }),
            json!("plain string"),
            json!({ "type": "text", "text": "hi" }),
        ] {
            assert!(media_content_blocks(&source_json_fixture(value)).is_none());
        }
    }

    #[test]
    fn source_mcp_text_block_preserves_lone_surrogates() {
        let mut input: Vec<u16> = r#"[{"type":"text","text":""#.encode_utf16().collect();
        input.push(0xd800);
        input.extend(r#""}]"#.encode_utf16());
        let data = Utf16JsonProjection::parse_utf16_json(&input).unwrap();

        let blocks = media_content_blocks(&data).unwrap();

        assert_eq!(blocks[0].string_units("/text"), Some(vec![0xd800]));
    }

    #[test]
    fn computer_batch_images_remain_ordered_and_do_not_project_for_unrelated_tools() {
        let data = source_json_fixture(
            json!({"stepsCompleted":3,"stepFailed":{"action":"key","error":"denied"},"results":[
                {"action":"screenshot","result":{"type":"image","file":{"base64":"first","type":"image/png"},"computer_frame":{"width":2}}},
                {"action":"left_click","result":{"ok":true}},
                {"action":"zoom","result":{"type":"image","file":{"base64":"second","type":"image/png"},"capture_region":[0,0,1,1]}}
            ]}),
        );
        let text = computer_batch_model_text(&data).unwrap();
        let text_value = text.string_at("").unwrap();
        assert!(!text_value.contains("base64"));
        let metadata = Utf16JsonProjection::parse(text_value).unwrap();
        assert_eq!(
            metadata.subprojection("/stepFailed").unwrap(),
            data.subprojection("/stepFailed").unwrap()
        );
        assert_eq!(
            metadata
                .subprojection("/results/0/result/computer_frame")
                .unwrap(),
            data.subprojection("/results/0/result/computer_frame")
                .unwrap()
        );
        let blocks = media_content_blocks_for_tool("computer", &data, &text).unwrap();
        assert_eq!(blocks[0].string_at("/text"), Some(text_value));
        assert_eq!(blocks[1].string_at("/text"), Some("[Step 1: screenshot]"));
        assert_eq!(blocks[2].string_at("/source/data"), Some("first"));
        assert_eq!(blocks[3].string_at("/text"), Some("[Step 3: zoom]"));
        assert_eq!(blocks[4].string_at("/source/data"), Some("second"));
        assert!(media_content_blocks_for_tool("Read", &data, &text).is_none());
        assert!(
            media_content_blocks_for_tool("mcp__computer-use__capture", &data, &text).is_none()
        );
        assert!(computer_batch_model_text(&source_json_fixture(
            json!({"stepsCompleted":1,"results":[{"action":"left_click","result":{"ok":true}}]})
        ))
        .is_none());
    }
}
