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

use serde_json::Value;

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
/// rendered PDF page, and the local-app view capture.
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
}

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
