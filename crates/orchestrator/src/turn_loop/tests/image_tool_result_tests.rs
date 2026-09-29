use super::image_tool_result_blocks;
use serde_json::json;

#[test]
fn image_result_becomes_the_binary_mapper_block_array() {
    let data = json!({
        "type": "image",
        "file": { "base64": "QUJD", "type": "image/png", "originalSize": 3 }
    });
    let blocks = image_tool_result_blocks(&data).expect("image data maps");
    // Byte shape of the binary mapper's `case "image"` — one image block,
    // source keys in written order (type, data, media_type).
    assert_eq!(
        serde_json::to_string(&blocks).unwrap(),
        r#"[{"type":"image","source":{"type":"base64","data":"QUJD","media_type":"image/png"}}]"#
    );
}

#[test]
fn bash_isimage_result_maps_stdout_data_uri_with_sniffed_media_type() {
    use super::bash_image_tool_result_blocks;
    // `iVBORw0KGgo=` = base64 of the 8-byte PNG magic. The URI CLAIMS jpeg —
    // the mapper must emit the SNIFFED type (image/png), per `hKn`/`Wfe`.
    let data = json!({
        "stdout": "data:image/jpeg;base64,iVBORw0KGgo=",
        "stderr": "",
        "interrupted": false,
        "isImage": true,
    });
    let blocks = bash_image_tool_result_blocks(&data).expect("sniffable data-URI maps");
    assert_eq!(
        serde_json::to_string(&blocks).unwrap(),
        r#"[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]"#
    );
}

#[test]
fn bash_isimage_misses_fall_back_to_none() {
    use super::bash_image_tool_result_blocks;
    // Not flagged as image.
    assert!(bash_image_tool_result_blocks(
        &json!({"stdout":"data:image/png;base64,iVBORw0KGgo=","isImage":false})
    )
    .is_none());
    // Flagged, but stdout is not a data-URI → text fallback (`hKn` null).
    assert!(
        bash_image_tool_result_blocks(&json!({"stdout":"plain text","isImage":true})).is_none()
    );
    // Valid URI shape but undecodable base64.
    assert!(bash_image_tool_result_blocks(
        &json!({"stdout":"data:image/png;base64,@@not-base64@@","isImage":true})
    )
    .is_none());
    // Decodable but unrecognized magic (claimed image, actually text bytes).
    assert!(bash_image_tool_result_blocks(
        &json!({"stdout":"data:image/png;base64,aGVsbG8gd29ybGQh","isImage":true})
    )
    .is_none());
}

#[test]
fn non_image_and_malformed_results_map_to_none() {
    // Strict no-op for every other tool result shape.
    assert!(image_tool_result_blocks(&json!({"type":"text","file":{}})).is_none());
    assert!(image_tool_result_blocks(&json!({"filePath":"/a","content":"x"})).is_none());
    assert!(image_tool_result_blocks(&json!("just a string")).is_none());
    // `type:"image"` but missing/malformed file fields → None (text fallback).
    assert!(image_tool_result_blocks(&json!({"type":"image"})).is_none());
    assert!(image_tool_result_blocks(&json!({"type":"image","file":{}})).is_none());
    assert!(image_tool_result_blocks(&json!({"type":"image","file":{"base64":"QQ=="}})).is_none());
}
