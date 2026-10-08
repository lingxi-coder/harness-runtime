//! Hash-pinned native result control within the dispatcher's typed domain.

use crate::tool_trait::tool_result_turn_end;

#[test]
fn typed_tool_turn_end_matches_2_1_286_native_helpers() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/tool_turn_end_2_1_286.json"
    ))
    .unwrap();
    assert_eq!(fixture["version"], "2.1.286");
    assert_eq!(
        fixture["binary_sha256"],
        "75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433"
    );

    let mut mapped = 0;
    let mut outside_domain = 0;
    for case in fixture["cases"].as_array().unwrap() {
        match case["domain"].as_str().unwrap() {
            "single_result_boolean" => {
                mapped += 1;
                let input = &case["helper_input"];
                let meta = input.get("mcp_meta").filter(|value| !value.is_null());
                let result = tool_result_turn_end(
                    input["tool_ends_turn"].as_bool().unwrap(),
                    input["is_error"].as_bool().unwrap(),
                    meta,
                );
                assert_eq!(
                    result.map(|marker| marker.source.as_str()),
                    case["expected_helper_source"].as_str(),
                    "native helper case {}",
                    case["name"]
                );
            }
            "raw_frame_only" => {
                outside_domain += 1;
                assert!(case.get("helper_input").is_none());
                assert!(!case["unmapped_reason"].as_str().unwrap().is_empty());
            }
            domain => panic!("unknown native fixture domain {domain}"),
        }
    }
    // Whole raw frames and truthy non-booleans are recorded native evidence,
    // never coerced into the dispatcher's single-result bool contract.
    assert_eq!((mapped, outside_domain), (28, 30));

    let markers = fixture["mcp_marker_cases"].as_array().unwrap();
    for case in markers {
        let result = tool_result_turn_end(false, false, Some(&case["input"]));
        let expected = case["expected"].as_bool().unwrap().then_some("mcp_meta");
        assert_eq!(
            result.map(|marker| marker.source.as_str()),
            expected,
            "native MCP marker case {}",
            case["name"]
        );
    }
    assert_eq!(markers.len(), 20);
}
