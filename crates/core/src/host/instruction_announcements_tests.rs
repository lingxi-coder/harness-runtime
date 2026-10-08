use super::*;
use crate::host::instruction_memory_sanitize::{js_trim, sanitize_memory_body};
use serde_json::Value;

fn oracle() -> Value {
    // Native fixtures keep their captured bytes; project the product instruction key here.
    serde_json::from_str(
        &include_str!("../../tests/fixtures/instruction_announcements_2_1_286.json")
            .replace("claudeMd", "instructions"),
    )
    .expect("pinned native announcement fixture")
}

fn branded(text: &str) -> String {
    text.replace("Claude Code", branding::PRODUCT_NAME)
}

#[test]
fn native_instruction_renderers_match_every_oracle_case() {
    let fixture = oracle();
    let cases = fixture["renderCases"].as_array().unwrap();
    assert!(cases.len() >= 29);
    for case in cases {
        let expected: Vec<_> = case["rendered"]
            .as_array()
            .unwrap()
            .iter()
            .map(|text| branded(text.as_str().unwrap()))
            .collect();
        let actual: Vec<_> = render_instruction_attachment(&case["attachment"])
            .into_iter()
            .collect();
        assert_eq!(actual, expected, "{}", case["name"]);
    }
}

#[test]
fn native_memory_sanitizer_and_js_trim_match_every_oracle_case() {
    let fixture = oracle();
    for case in fixture["sanitizeCases"].as_array().unwrap() {
        assert_eq!(
            sanitize_memory_body(case["input"].as_str().unwrap()),
            case["expected"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
    for case in fixture["trimCases"].as_array().unwrap() {
        assert_eq!(
            js_trim(case["input"].as_str().unwrap()),
            case["expected"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn native_ordinary_context_announcements_match_oracle_order_and_deltas() {
    let fixture = oracle();
    let mut ordinary = 0;
    let mut inline = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let mut context = InstructionContext::default();
        for field in ["userContext", "systemContext"] {
            if let Some(values) = case[field].as_object() {
                for (name, text) in values {
                    if name == "cacheBreaker" {
                        continue;
                    }
                    if !context.user_context.contains_key(name) {
                        context.user_context_order.push(name.clone());
                        context
                            .user_context
                            .insert(name.clone(), text.as_str().unwrap().into());
                    }
                }
            }
        }
        context.managed_instructions_only = case["options"]["managedInstructionsOnly"] == true;
        if let Some(files) = case.get("current") {
            let mut files: Vec<InstructionFile> = serde_json::from_value(files.clone()).unwrap();
            for file in &mut files {
                file.content = js_trim(&file.content).into();
            }
            if context.managed_instructions_only {
                files.retain(|file| file.kind == InstructionFileType::Managed);
            }
            context.eager_instructions = Some(files);
        }
        context.announcement_history = case["prior"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("attachment").cloned())
                    .collect()
            })
            .unwrap_or_default();
        if case["routing"]["bare"] == true {
            // Or has selected its separate inline API. Its generated prefix
            // never enters the announcement history passed above.
            context.user_context =
                serde_json::from_value(case["routing"]["userContext"].clone()).unwrap();
            let actual: Vec<_> = context.reminder().into_iter().collect();
            let expected: Vec<_> = case["inlinePrefix"]
                .as_array()
                .unwrap()
                .iter()
                .map(|text| branded(text.as_str().unwrap()))
                .collect();
            assert_eq!(actual, expected, "{}", case["name"]);
            inline += 1;
            continue;
        }
        if case["resumeActive"] == true {
            // Br suppresses all announcements for an incomplete-thinking API
            // recovery, a caller guard distinct from cold Agent resume.
            assert!(case["expected"].as_array().unwrap().is_empty());
            continue;
        }
        let date = case["date"].as_str().unwrap_or("2026-09-30");
        let reason = case["reason"].as_str().unwrap_or("session_start");
        let actual = context_attachments(&context, date, reason);
        let expected: Vec<_> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["attachment"].clone())
            .collect();
        assert_eq!(actual, expected, "{}", case["name"]);
        let actual_rendered: Vec<_> = actual
            .iter()
            .flat_map(render_instruction_attachment)
            .collect();
        let expected_rendered: Vec<_> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| entry["rendered"].as_array().unwrap())
            .map(|text| branded(text.as_str().unwrap()))
            .collect();
        assert_eq!(actual_rendered, expected_rendered, "{}", case["name"]);
        ordinary += 1;
    }
    assert_eq!(ordinary, 17);
    assert_eq!(inline, 3);
}

#[test]
fn latest_raw_snapshot_hints_match_native_family_schema() {
    let fixture = oracle();
    let cases = fixture["snapshotCases"].as_array().unwrap();
    assert_eq!(cases.len(), 26);
    for case in cases {
        let attachments: Vec<_> = case["prior"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row.get("attachment").cloned())
            .collect();
        let expected = match case["expected"]["snapshotHint"].as_str() {
            Some("inline") => Some(InstructionRendering::Inline),
            Some("announced") => Some(InstructionRendering::Announced),
            _ => None,
        };
        assert_eq!(
            latest_snapshot_context_rendering(&attachments),
            expected,
            "{}",
            case["name"]
        );
    }
}
