//! Byte locks from the unchanged Claude Code 2.1.321 chain writer.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use session::jsonl::exact_json::native_message_bytes;
use session::JsonlMessage;

#[test]
fn newly_constructed_rows_match_native_321_envelope_bytes() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/session_rows_2_1_321.json")).unwrap();
    let mut checked = 0;
    for case in fixture["cases"].as_array().unwrap() {
        if case["fresh"] != true {
            continue;
        }
        for expected in case["expected"]["rows"].as_array().unwrap() {
            let source = expected.as_str().unwrap();
            let projection = Utf16JsonProjection::parse(source).unwrap();
            let mut message = JsonlMessage::from_projection(&projection).unwrap();
            // New in-memory messages have no reader-origin key-order receipt.
            // Removing it tests the production serializer's native envelope.
            message
                .extra
                .remove_field("lingxi_jsonl_source_key_order")
                .unwrap();
            assert_eq!(
                String::from_utf8(native_message_bytes(&message).unwrap()).unwrap(),
                source,
                "native case: {}",
                case["name"]
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 16,
        "all fresh native row cases must be exercised"
    );
}

#[test]
fn restored_rows_retain_native_321_order_strings_and_version_presence() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/session_rows_2_1_321.json")).unwrap();
    let mut checked = 0;
    let mut saw_absent_version = false;
    for case in fixture["cases"].as_array().unwrap() {
        for expected in case["expected"]["rows"].as_array().unwrap() {
            let source = expected.as_str().unwrap();
            let projection = Utf16JsonProjection::parse(source).unwrap();
            // Normal recordTranscript filters progress before the chain writer.
            // The oracle includes one only to establish parent advancement.
            if projection.string_at("/type") == Some("progress") {
                continue;
            }
            saw_absent_version |= !projection.contains_property("version");
            let row = JsonlMessage::from_projection(&projection).unwrap();
            assert_eq!(
                String::from_utf8(native_message_bytes(&row).unwrap()).unwrap(),
                source,
                "native case: {}",
                case["name"]
            );
            checked += 1;
        }
    }
    assert!(saw_absent_version);
    assert!(checked >= 20);
}

#[test]
fn native_321_fork_titles_remain_catalogued_after_utf16_slicing() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/session_fork_2_1_321.json")).unwrap();
    let sid = fixture["targetSessionId"].as_str().unwrap();
    let mut checked_surrogate = false;
    for case in fixture["forkCases"].as_array().unwrap() {
        let source = case["expectedJsonl"].as_str().unwrap();
        let title_rows = source
            .lines()
            .filter(|line| {
                let row = Utf16JsonProjection::parse(line).unwrap();
                matches!(row.string_at("/type"), Some("custom-title" | "agent-name"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let loaded = session::jsonl::route_lines(&title_rows);
        assert_eq!(loaded.malformed_line_count, 0, "{}", case["name"]);
        let title = Utf16JsonProjection::parse(title_rows.lines().next().unwrap()).unwrap();
        let units = title.string_units("/customTitle").unwrap();
        checked_surrogate |= String::from_utf16(&units).is_err();
        let display = String::from_utf16_lossy(&units);
        assert_eq!(loaded.custom_titles.get(sid), Some(&display));
        assert_eq!(loaded.agent_names.get(sid), Some(&display));
    }
    assert!(checked_surrogate, "fixture must cover lone-surrogate names");

    let loaded = session::jsonl::route_lines(
        r#"{"type":"ai-title","sessionId":"s","aiTitle":"\ud83d"}
{"type":"summary","leafUuid":"leaf","summary":"\ude00"}"#,
    );
    assert_eq!(loaded.malformed_line_count, 0);
    assert_eq!(loaded.ai_titles["s"], "\u{fffd}");
    assert_eq!(loaded.summaries["leaf"], "\u{fffd}");
}
