//! Preserve native normalizer bytes, rejection priority and candidate ordering.
#[allow(dead_code)]
#[path = "../src/conversation/prompt_suggestion_filter.rs"]
mod filter;
use filter::{parse_prompt_suggestion_response, prompt_suggestion_suppression_reason};

fn normalize_prompt_suggestion_response(raw: &str) -> String {
    let raw = lingxi_core::types::utf16_json::Utf16JsonProjection::from(raw);
    let normalized = filter::normalize_prompt_suggestion_projection(&raw)
        .expect("prompt suggestion fixture input is a root string");
    let units = normalized
        .string_units("")
        .expect("normalized suggestion remains a root string");
    String::from_utf16_lossy(&units)
}

#[test]
fn native_normalization_and_suppression_reasons() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/prompt_suggestion_2_1_296.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let raw = case["raw"].as_str().unwrap();
        let normalized = normalize_prompt_suggestion_response(raw);
        assert_eq!(normalized, case["normalized"].as_str().unwrap(), "{case}");
        assert_eq!(
            prompt_suggestion_suppression_reason(&normalized),
            case["reason"].as_str(),
            "{case}"
        );
        assert_eq!(
            parse_prompt_suggestion_response(raw).as_deref(),
            case["expected"].as_str(),
            "{case}"
        );
    }
    for case in fixture["candidateCases"].as_array().unwrap() {
        let selected = case["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| normalize_prompt_suggestion_response(v.as_str().unwrap()))
            .find(|v| !v.is_empty());
        assert_eq!(selected.as_deref(), case["selected"].as_str(), "{case}");
        let reason = selected
            .as_deref()
            .and_then(prompt_suggestion_suppression_reason);
        assert_eq!(reason, case["reason"].as_str(), "{case}");
        let accepted = selected.filter(|_| reason.is_none());
        assert_eq!(accepted.as_deref(), case["expected"].as_str(), "{case}");
    }
}
