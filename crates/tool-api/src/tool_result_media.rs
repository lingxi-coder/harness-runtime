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
//! The app view-capture tool therefore worked in a normal conversation and returned
//! ~230 KB of base64 as plain TEXT to the `frontend-qa` verify subagent, which
//! is the only caller the capture tool exists for. The subagent then reported
//! `render_check.status = "passed"` — honestly, from metadata — for a frame it
//! had never been able to look at.

use serde_json::Value;

/// Tool-aware media projection shared by the main and child loops.
#[must_use]
pub fn media_content_blocks_for_tool(
    name: &str,
    data: &Value,
    model_text: &str,
) -> Option<Vec<Value>> {
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

fn computer_batch_images(data: &Value) -> Option<Vec<(usize, Vec<Value>)>> {
    data.get("stepsCompleted")?.as_u64()?;
    let images: Vec<_> = data
        .get("results")?
        .as_array()?
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item["action"].as_str(), Some("screenshot" | "zoom")))
        .filter_map(|(index, item)| {
            image_content_blocks(&item["result"]).map(|blocks| (index, blocks))
        })
        .collect();
    (!images.is_empty()).then_some(images)
}

/// Keep Computer's batch wrapper and geometry visible without spelling out
/// image bytes in the accompanying text. The raw result still reaches hooks.
#[must_use]
pub fn computer_batch_model_text(data: &Value) -> Option<String> {
    let images = computer_batch_images(data)?;
    let mut metadata = data.clone();
    for (index, _) in images {
        metadata["results"][index]["result"]["file"]
            .as_object_mut()?
            .remove("base64");
    }
    Some(crate::native_schema::js_json(&metadata, false))
}

fn computer_batch_content_blocks(data: &Value, model_text: &str) -> Option<Vec<Value>> {
    let images = computer_batch_images(data)?;
    let mut blocks = vec![serde_json::json!({"type":"text","text":model_text})];
    for (index, image) in images {
        blocks.push(serde_json::json!({"type":"text","text":format!(
            "[Step {}: {}]", index + 1, data["results"][index]["action"].as_str()?
        )}));
        blocks.extend(image);
    }
    Some(blocks)
}

/// Read's notebook, PDF, and extracted-page result mapper. The summary must be
/// the same tool-owned text used for an ordinary result; blocks replace it on
/// the wire.
#[must_use]
pub fn read_media_content_blocks(data: &Value, model_text: &str) -> Option<Vec<Value>> {
    let file = data.get("file")?;
    match data.get("type")?.as_str()? {
        "notebook" => {
            let cells = file.get("cells")?.as_array()?;
            let mut blocks: Vec<Value> = Vec::new();
            for cell in cells {
                let cell_type = cell.get("cellType")?.as_str()?;
                let cell_id = cell.get("cell_id")?.as_str()?;
                let source = cell.get("source")?.as_str()?;
                let mut metadata = String::new();
                if cell_type != "code" {
                    metadata.push_str(&format!("<cell_type>{cell_type}</cell_type>"));
                } else if let Some(language) = cell.get("language").and_then(Value::as_str) {
                    if language != "python" {
                        metadata.push_str(&format!("<language>{language}</language>"));
                    }
                }
                append_read_text_block(
                    &mut blocks,
                    format!("<cell id=\"{cell_id}\">{metadata}{source}</cell id=\"{cell_id}\">"),
                );
                if let Some(outputs) = cell.get("outputs").and_then(Value::as_array) {
                    for output in outputs {
                        if let Some(text) = output.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                append_read_text_block(&mut blocks, format!("\n{text}"));
                            }
                        }
                        if let Some(image) = output.get("image") {
                            let base64 = image.get("image_data")?.as_str()?;
                            let media_type = image.get("media_type")?.as_str()?;
                            blocks.push(serde_json::json!({
                                "type":"image",
                                "source":{"type":"base64","data":base64,"media_type":media_type}
                            }));
                        }
                    }
                }
            }
            Some(blocks)
        }
        "pdf" => {
            let base64 = file.get("base64")?.as_str()?;
            if base64.is_empty() {
                return None;
            }
            Some(vec![
                serde_json::json!({"type":"text","text":model_text}),
                serde_json::json!({
                    "type":"document",
                    "source":{"type":"base64","media_type":"application/pdf","data":base64}
                }),
            ])
        }
        "parts" => {
            let pages = data.get("pages")?.as_array()?;
            if pages.is_empty() {
                return None;
            }
            let first_page = data.get("firstPage").and_then(Value::as_u64).unwrap_or(1);
            let mut blocks = vec![serde_json::json!({"type":"text","text":model_text})];
            for (index, page) in pages.iter().enumerate() {
                let base64 = page.get("base64")?.as_str()?;
                if !base64.is_empty() {
                    let media_type = page.get("mediaType")?.as_str()?;
                    blocks.push(serde_json::json!({
                        "type":"image",
                        "source":{"type":"base64","data":base64,"media_type":media_type}
                    }));
                } else {
                    let suffix = page
                        .get("error")
                        .and_then(Value::as_str)
                        .filter(|error| !error.is_empty())
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default();
                    blocks.push(serde_json::json!({
                        "type":"text",
                        "text":format!("[Page {} could not be processed as an image{}]", first_page.saturating_add(index as u64), suffix)
                    }));
                }
            }
            Some(blocks)
        }
        _ => None,
    }
}

fn append_read_text_block(blocks: &mut Vec<Value>, text: String) {
    if let Some(last) = blocks.last_mut() {
        if let Some(previous) = last.get("text").and_then(Value::as_str) {
            let merged = format!("{previous}\n{text}");
            *last = serde_json::json!({"type":"text","text":merged});
            return;
        }
    }
    blocks.push(serde_json::json!({"type":"text","text":text}));
}

/// Claude's Bash `isImage` mapper: require a base64 data URI and sniff the
/// decoded payload instead of trusting the URI's claimed media type.
#[must_use]
pub fn bash_image_content_blocks(data: &Value) -> Option<Vec<Value>> {
    use base64::Engine as _;
    if data.get("isImage") != Some(&Value::Bool(true)) {
        return None;
    }
    let stdout = data.get("stdout").and_then(Value::as_str)?;
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
    Some(vec![serde_json::json!({
        "type": "image",
        "source": {"type":"base64","media_type":media_type,"data":payload},
    })])
}

/// The media blocks a tool result's `data` should be delivered as, or `None`
/// when it is an ordinary textual result.
///
/// Deliberately narrow. Every shape here is one the result mapper already
/// produces; an unrecognized payload falls through to `None` so the wire form
/// stays exactly the text it was.
#[must_use]
pub fn media_content_blocks(data: &Value) -> Option<Vec<Value>> {
    // An MCP tool answers with the content array verbatim — it is already in
    // block form, including any image blocks the server sent.
    if let Some(blocks) = data.as_array() {
        return (!blocks.is_empty()).then(|| blocks.clone());
    }
    image_content_blocks(data)
}

/// The `{type:"image", file:{base64, type}}` shape: `Read` on an image file, a
/// rendered PDF page, and the app view capture.
#[must_use]
pub fn image_content_blocks(data: &Value) -> Option<Vec<Value>> {
    if data.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let file = data.get("file")?;
    let base64 = file.get("base64").and_then(Value::as_str)?;
    // An empty payload is NOT an image. Emitting a block with `data: ""` sends
    // the model a corrupt attachment instead of a readable failure, and the
    // text arm still has the error the tool actually returned.
    if base64.is_empty() {
        return None;
    }
    let media_type = file.get("type").and_then(Value::as_str)?;
    Some(vec![serde_json::json!({
        "type": "image",
        "source": {
            "type": "base64",
            "data": base64,
            "media_type": media_type,
        },
    })])
}

/// The text that must accompany a media result, when the payload asks not to be
/// spelled out.
///
/// Without this the caller stringifies the whole `data` — base64 included — into
/// the tool_result text that lives in session history for the rest of the run.
/// Four captured frames is roughly a megabyte of duplicated payload next to the
/// image blocks the model is actually reading.
#[must_use]
pub fn ephemeral_summary(data: &Value) -> Option<String> {
    if data.get("_lingxi_ephemeral").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    data.get("summary")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_captured_frame_becomes_an_image_block() {
        let data = serde_json::json!({
            "type": "image",
            "file": { "base64": "AAAA", "type": "image/jpeg" },
        });
        let blocks = media_content_blocks(&data).expect("an image result carries blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["data"], "AAAA");
        assert_eq!(blocks[0]["source"]["media_type"], "image/jpeg");
    }

    /// The failure this guards is not "no image" — it is a CORRUPT one. A block
    /// with empty data reaches the model as an unreadable attachment and hides
    /// the error text that would have explained the capture failure.
    #[test]
    fn an_empty_payload_is_not_an_image() {
        let data = serde_json::json!({
            "type": "image",
            "file": { "base64": "", "type": "image/jpeg" },
        });
        assert!(media_content_blocks(&data).is_none());
    }

    #[test]
    fn a_missing_media_type_is_not_an_image() {
        let data = serde_json::json!({ "type": "image", "file": { "base64": "AAAA" } });
        assert!(media_content_blocks(&data).is_none());
    }

    #[test]
    fn an_mcp_array_passes_through_and_an_empty_one_does_not() {
        let data = serde_json::json!([{ "type": "text", "text": "hi" }]);
        assert_eq!(
            media_content_blocks(&data)
                .expect("array passes through")
                .len(),
            1
        );
        assert!(media_content_blocks(&serde_json::json!([])).is_none());
    }

    #[test]
    fn ordinary_results_stay_text() {
        for data in [
            serde_json::json!({ "ok": true }),
            serde_json::json!("plain string"),
            serde_json::json!({ "type": "text", "text": "hi" }),
            Value::Null,
        ] {
            assert!(
                media_content_blocks(&data).is_none(),
                "{data} must stay text"
            );
        }
    }

    #[test]
    fn only_a_marked_payload_offers_a_compact_summary() {
        assert_eq!(
            ephemeral_summary(&serde_json::json!({
                "_lingxi_ephemeral": true,
                "summary": "one frame",
            }))
            .as_deref(),
            Some("one frame")
        );
        assert!(ephemeral_summary(&serde_json::json!({ "summary": "one frame" })).is_none());
        assert!(ephemeral_summary(&serde_json::json!({ "_lingxi_ephemeral": true })).is_none());
    }

    #[test]
    fn bash_image_mapper_sniffs_bytes_and_only_runs_for_bash() {
        let data = serde_json::json!({
            "isImage":true,
            "stdout":"data:image/jpeg;base64,iVBORw0KGgo="
        });
        let blocks = media_content_blocks_for_tool("Bash", &data, "ignored").unwrap();
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "iVBORw0KGgo=");
        assert!(media_content_blocks_for_tool("Read", &data, "ignored").is_none());
    }

    #[test]
    fn computer_batch_images_remain_ordered_and_do_not_project_for_unrelated_tools() {
        let data = serde_json::json!({"stepsCompleted":3,"stepFailed":{"action":"key","error":"denied"},"results":[
            {"action":"screenshot","result":{"type":"image","file":{"base64":"first","type":"image/png"},"computer_frame":{"width":2}}},
            {"action":"left_click","result":{"ok":true}},
            {"action":"zoom","result":{"type":"image","file":{"base64":"second","type":"image/png"},"capture_region":[0,0,1,1]}}
        ]});
        let text = computer_batch_model_text(&data).unwrap();
        assert!(!text.contains("base64"));
        let metadata: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(metadata["stepFailed"], data["stepFailed"]);
        assert_eq!(
            metadata["results"][0]["result"]["computer_frame"],
            data["results"][0]["result"]["computer_frame"]
        );
        let blocks = media_content_blocks_for_tool("computer", &data, &text).unwrap();
        assert_eq!(blocks[0]["text"], text);
        assert_eq!(blocks[1]["text"], "[Step 1: screenshot]");
        assert_eq!(blocks[2]["source"]["data"], "first");
        assert_eq!(blocks[3]["text"], "[Step 3: zoom]");
        assert_eq!(blocks[4]["source"]["data"], "second");
        assert!(media_content_blocks_for_tool("Read", &data, &text).is_none());
        assert!(
            media_content_blocks_for_tool("mcp__computer-use__capture", &data, &text).is_none()
        );
        assert!(computer_batch_model_text(&serde_json::json!({"stepsCompleted":1,"results":[{"action":"left_click","result":{"ok":true}}]})).is_none());
    }

    #[test]
    fn read_pdf_replacement_keeps_summary_and_document_in_one_result() {
        let data = serde_json::json!({"type":"pdf","file":{"base64":"JVBERi0="}});
        let blocks =
            media_content_blocks_for_tool("Read", &data, "PDF file read: x (5 B)").unwrap();
        assert_eq!(
            blocks[0],
            serde_json::json!({"type":"text","text":"PDF file read: x (5 B)"})
        );
        assert_eq!(blocks[1]["type"], "document");
        assert_eq!(blocks[1]["source"]["data"], "JVBERi0=");
        assert!(read_media_content_blocks(
            &serde_json::json!({"type":"pdf","file":{"base64":""}}),
            "x"
        )
        .is_none());
    }

    #[test]
    fn read_pages_replacement_interleaves_images_and_page_failures() {
        let data = serde_json::json!({
            "type":"parts", "firstPage":3, "file":{},
            "pages":[
                {"base64":"aGVsbG8=","mediaType":"image/jpeg"},
                {"base64":"","mediaType":"image/jpeg","error":"decode failed"}
            ]
        });
        let blocks =
            media_content_blocks_for_tool("Read", &data, "PDF pages extracted: 2").unwrap();
        assert_eq!(blocks[0]["text"], "PDF pages extracted: 2");
        assert_eq!(blocks[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(
            blocks[2]["text"],
            "[Page 4 could not be processed as an image: decode failed]"
        );
        assert!(read_media_content_blocks(
            &serde_json::json!({"type":"parts","file":{},"pages":[]}),
            "x"
        )
        .is_none());
    }

    #[test]
    fn read_notebook_replacement_keeps_image_between_text_blocks() {
        let data = serde_json::json!({"type":"notebook","file":{"cells":[
            {"cellType":"code","cell_id":"cell-0","source":"print(1)","language":"python","outputs":[
                {"text":"1","image":{"image_data":"aGVsbG8=","media_type":"image/png"}},
                {"text":"done"}
            ]}
        ]}});
        let blocks = media_content_blocks_for_tool("Read", &data, "ignored").unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            blocks[0]["text"],
            "<cell id=\"cell-0\">print(1)</cell id=\"cell-0\">\n\n1"
        );
        assert_eq!(blocks[1]["source"]["data"], "aGVsbG8=");
        assert_eq!(blocks[2]["text"], "\ndone");
    }
}
