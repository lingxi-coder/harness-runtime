//! Differential coverage and a manual scaling probe for notebook media text.

use super::*;
use std::cell::Cell;

thread_local! { static RECOPIED_UNITS: Cell<usize> = const { Cell::new(0) }; }

// Pre-optimization algorithm, retained only for differential tests.
fn legacy_read_media_content_blocks(
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
            legacy_append_read_text_block(&mut blocks, cell_text);

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
                                legacy_append_read_text_block(&mut blocks, line);
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

fn legacy_append_read_text_block(blocks: &mut Vec<Utf16JsonProjection>, text: Vec<u16>) {
    if let Some(last) = blocks.last_mut() {
        if let Some(previous) = last
            .get_property("text")
            .and_then(JsonValueView::code_units)
        {
            RECOPIED_UNITS.with(|count| count.set(count.get() + previous.len()));
            let mut merged = previous.to_vec();
            merged.push(u16::from(b'\n'));
            merged.extend(text);
            *last = text_block(source_text_projection_units(&merged));
            return;
        }
    }
    blocks.push(text_block(source_text_projection_units(&text)));
}

fn block_bytes(blocks: Option<Vec<Utf16JsonProjection>>) -> Option<Vec<String>> {
    blocks.map(|blocks| {
        blocks
            .iter()
            .map(|block| block.to_json_string().unwrap())
            .collect()
    })
}

#[test]
fn notebook_text_accumulator_preserves_empty_blocks_and_lone_units() {
    let mut legacy = Vec::new();
    let mut optimized = Vec::new();
    let mut pending = None;
    for units in [vec![], vec![0xd800], vec![], vec![0xdc00]] {
        legacy_append_read_text_block(&mut legacy, units.clone());
        append_read_text_block(&mut pending, units);
    }
    flush_read_text_block(&mut optimized, &mut pending);
    assert_eq!(block_bytes(Some(optimized)), block_bytes(Some(legacy)));

    let mut blocks = Vec::new();
    let mut pending = Some(Vec::new());
    flush_read_text_block(&mut blocks, &mut pending);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].string_units("/text"), Some(Vec::new()));
    flush_read_text_block(&mut blocks, &mut pending);
    assert_eq!(blocks.len(), 1);
}

#[test]
fn notebook_text_accumulator_matches_legacy_at_image_barriers() {
    for raw in [
        r#"{"type":"notebook","file":{"cells":[]}}"#,
        r#"{"type":"notebook","file":{"cells":[{"cellType":"code","cell_id":"first","source":"","outputs":[{"text":""},{"text":"one"},{"text":"two","image":{"image_data":"first","media_type":"image/png"}},{"text":"after image"},{"image":{"image_data":"","media_type":"image/png"}}]},{"cellType":"markdown","cell_id":"second","source":"tail","outputs":[{"text":"last"}]}]}}"#,
        r#"{"type":"notebook","file":{"cells":[{"cellType":"code","language":"py\ud800","cell_id":"id\udc00","source":"source\ud800","outputs":[{"text":"out\udc00","image":{"image_data":"data\ud800","media_type":"type\udc00"}},{"text":"final\ud800"}]},{"cellType":"raw\ud800","cell_id":"second","source":"end\udc00"}]}}"#,
        r#"{"type":"notebook","file":{"cells":[{"cellType":"code","cell_id":"first","source":"before"},{"cellType":"code","cell_id":"bad"}]}}"#,
    ] {
        let data = Utf16JsonProjection::parse(raw).unwrap();
        let model_text = source_text_projection("summary");
        assert_eq!(
            block_bytes(read_media_content_blocks(&data, &model_text)),
            block_bytes(legacy_read_media_content_blocks(&data, &model_text)),
            "{raw}"
        );
    }
}

#[test]
#[ignore = "manual scaling probe; prints timings without timing assertions"]
fn notebook_text_merge_scaling_probe() {
    for count in [128usize, 512, 2048] {
        let cells: Vec<_> = (0..count).map(|index| serde_json::json!({"cellType":"code","cell_id":format!("c{index}"),"source":"print(1)","outputs":[{"text":"result"}]})).collect();
        let data = Utf16JsonProjection::parse(
            &serde_json::json!({"type":"notebook","file":{"cells":cells}}).to_string(),
        )
        .unwrap();
        let model_text = source_text_projection("summary");
        RECOPIED_UNITS.with(|count| count.set(0));
        let started = std::time::Instant::now();
        let before = legacy_read_media_content_blocks(&data, &model_text);
        let legacy_us = started.elapsed().as_micros();
        let recopied_units = RECOPIED_UNITS.with(Cell::get);
        let started = std::time::Instant::now();
        let after = read_media_content_blocks(&data, &model_text);
        let optimized_us = started.elapsed().as_micros();
        assert_eq!(block_bytes(after), block_bytes(before));
        println!("{{\"probe\":\"notebook_text_merge\",\"cells\":{count},\"legacy_prefix_recopied_utf16_units\":{recopied_units},\"optimized_prefix_recopied_utf16_units\":0,\"legacy_us\":{legacy_us},\"optimized_us\":{optimized_us}}}");
    }
}
