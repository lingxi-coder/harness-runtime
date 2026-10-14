use super::*;
use crate::host::instruction_memory_sanitize::{js_trim, sanitize_memory_body};
use serde_json::Value;

fn oracle() -> Value {
    // Keep the captured 2.1.286 asset immutable; only adapt its historical
    // internal context key for this current-contract behavior regression.
    let native = include_str!("../../tests/fixtures/instruction_announcements_2_1_286.json")
        .replace("claudeMd", "instructions");
    serde_json::from_str(&native).expect("pinned native announcement fixture")
}

fn projection(value: &Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(value).unwrap()
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
        let attachment = projection(&case["attachment"]);
        let actual: Vec<_> = render_instruction_attachment(&attachment)
            .into_iter()
            .map(|rendered| rendered.string_at("").expect("rendered string").to_owned())
            .collect();
        assert_eq!(actual, expected, "{}", case["name"]);
    }
}

#[test]
fn native_memory_sanitizer_and_js_trim_match_every_oracle_case() {
    let fixture = oracle();
    for case in fixture["sanitizeCases"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let expected = case["expected"].as_str().unwrap();
        assert_eq!(sanitize_memory_body(input), expected, "{}", case["name"]);
        assert_eq!(
            super::super::instruction_memory_sanitize::sanitize_memory_body_utf16(
                &input.encode_utf16().collect::<Vec<_>>(),
            ),
            expected.encode_utf16().collect::<Vec<_>>(),
            "UTF-16 AutoMem source variant: {}",
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
                    let key = name.encode_utf16().collect::<Vec<_>>();
                    if context.user_context_value(&key).is_none() {
                        context
                            .set_user_context_text(name, text.as_str().unwrap())
                            .unwrap();
                    }
                }
            }
        }
        context.managed_instructions_only = case["options"]["managedInstructionsOnly"] == true;
        if let Some(files) = case.get("current") {
            let mut files: Vec<InstructionFile> = serde_json::from_str(&files.to_string()).unwrap();
            for file in &mut files {
                let content = file.content.string_at("").unwrap();
                file.content = Utf16JsonProjection::from(js_trim(content));
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
                    .filter_map(|row| row.get("attachment"))
                    .map(projection)
                    .collect()
            })
            .unwrap_or_default();
        if case["routing"]["bare"] == true {
            // Or has selected its separate inline API. Its generated prefix
            // never enters the announcement history passed above.
            context.user_context = projection(&case["routing"]["userContext"]);
            let actual: Vec<_> = context
                .reminder_source()
                .into_iter()
                .map(|rendered| rendered.string_at("").expect("rendered string").to_owned())
                .collect();
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
            .map(|entry| projection(&entry["attachment"]))
            .collect();
        // The oracle's JSON objects are structural payload fixtures. Preserve
        // attachment array order, while comparing exact UTF-16 members without
        // treating flat AST allocation/property insertion order as JSON value
        // equality. Native process byte ordering requires a separate capture.
        assert_eq!(actual.len(), expected.len(), "{}", case["name"]);
        for (actual, expected) in actual.iter().zip(&expected) {
            assert!(actual.semantically_eq(expected), "{}", case["name"]);
        }
        let actual_rendered: Vec<_> = actual
            .iter()
            .flat_map(render_instruction_attachment)
            .map(|rendered| rendered.string_at("").expect("rendered string").to_owned())
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
            .filter_map(|row| row.get("attachment"))
            .map(projection)
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

#[test]
fn instruction_context_and_eager_file_round_trip_lone_utf16_through_rendering() {
    assert!(serde_json::from_str::<InstructionFile>(
        r#"{"path":{},"type":"Managed","content":"body"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<InstructionContext>(r#"{"user_context":null}"#).is_err());

    let path_units = vec![b'<' as u16, 0xd800, b'>' as u16];
    let content_units = vec![b'A' as u16, 0xd800, b'B' as u16];
    let file = InstructionFile::new(
        Utf16JsonProjection::root_string(String::from_utf16_lossy(&path_units), path_units.clone())
            .unwrap(),
        InstructionFileType::Managed,
        Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&content_units),
            content_units.clone(),
        )
        .unwrap(),
    );

    let serialized_file = serde_json::to_string(&file).unwrap();
    assert!(serialized_file.contains(r#""path":"<\ud800>""#));
    assert!(serialized_file.contains(r#""content":"A\ud800B""#));
    let restored_file: InstructionFile = serde_json::from_str(&serialized_file).unwrap();
    assert_eq!(restored_file.path_units(), path_units);
    assert_eq!(restored_file.content_units(), content_units);

    let attachment = instructions_delta(&[restored_file], None, None).unwrap();
    assert!(is_valid_context_attachment(&attachment));
    let serialized_attachment = attachment.to_json_string().unwrap();
    let cold_attachment = Utf16JsonProjection::parse(&serialized_attachment).unwrap();
    let recovered = recover_instruction_files(std::slice::from_ref(&cold_attachment)).unwrap();
    assert_eq!(
        recovered[0].path_units(),
        [b'<' as u16, 0xd800, b'>' as u16]
    );
    assert_eq!(
        recovered[0].content_units(),
        [b'A' as u16, 0xd800, b'B' as u16]
    );
    let rendered = render_instruction_attachment(&cold_attachment).unwrap();
    let rendered_units = rendered.string_units("").unwrap();
    assert!(rendered_units
        .windows(3)
        .any(|window| window == [b'A' as u16, 0xd800, b'B' as u16]));
    let removed = instructions_delta(&[], Some(&recovered), Some("directory_added")).unwrap();
    assert!(removed
        .to_json_string()
        .unwrap()
        .contains(r#""removed":["<\ud800>"]"#));
    let cold_removed = Utf16JsonProjection::parse(&removed.to_json_string().unwrap()).unwrap();
    assert!(
        recover_instruction_files(&[cold_attachment.clone(), cold_removed.clone()])
            .unwrap()
            .is_empty()
    );
    assert!(render_instruction_attachment(&cold_removed)
        .unwrap()
        .string_units("")
        .unwrap()
        .windows(3)
        .any(|window| window == [b'<' as u16, 0xd800, b'>' as u16]));

    let mut context = InstructionContext::default();
    context.set_user_context_text("before", "old").unwrap();
    context.set_user_context_text("before", "updated").unwrap();
    context
        .set_user_context_value(
            &[0xd801],
            Utf16JsonProjection::root_string(
                String::from_utf16_lossy(&[b'X' as u16, 0xdc01]),
                vec![b'X' as u16, 0xdc01],
            )
            .unwrap(),
        )
        .unwrap();
    context.set_user_context_text("after", "tail").unwrap();
    let serialized_context = serde_json::to_string(&context).unwrap();
    assert!(serialized_context
        .contains(r#""user_context":{"before":"updated","\ud801":"X\udc01","after":"tail"}"#));
    let restored_context: InstructionContext = serde_json::from_str(&serialized_context).unwrap();
    assert_eq!(
        restored_context
            .user_context_entries()
            .into_iter()
            .map(|(key, value)| (key, value.string_units("")))
            .collect::<Vec<_>>(),
        vec![
            (
                "before".encode_utf16().collect(),
                Some("updated".encode_utf16().collect())
            ),
            (vec![0xd801], Some(vec![b'X' as u16, 0xdc01])),
            (
                "after".encode_utf16().collect(),
                Some("tail".encode_utf16().collect())
            ),
        ]
    );
    restored_context
        .user_context
        .subprojection_property_units(&[0xd801])
        .expect("lone-surrogate property name survives cold restore");
    let reminder = restored_context.reminder_source().unwrap();
    let reminder_units = reminder.string_units("").unwrap();
    assert!(reminder_units
        .windows(2)
        .any(|window| window == [b'X' as u16, 0xdc01]));
    assert!(reminder_units.windows(1).any(|window| window == [0xd801]));
}

#[test]
fn native_user_context_entries_use_javascript_integer_key_order_without_mutating_source() {
    let mut context = InstructionContext::default();
    let inserted = [
        ("10", "ten"),
        ("before", "before"),
        ("01", "zero-padded"),
        ("2", "two"),
        ("4294967295", "not-an-index"),
        ("0", "zero"),
        ("4294967294", "last-array-index"),
    ];
    for (key, value) in inserted {
        context.set_user_context_text(key, value).unwrap();
    }
    context
        .set_user_context_value(&[0xd800], Utf16JsonProjection::from("lone-key"))
        .unwrap();

    let expected_source_keys = vec![
        "10".encode_utf16().collect::<Vec<_>>(),
        "before".encode_utf16().collect(),
        "01".encode_utf16().collect(),
        "2".encode_utf16().collect(),
        "4294967295".encode_utf16().collect(),
        "0".encode_utf16().collect(),
        "4294967294".encode_utf16().collect(),
        vec![0xd800],
    ];
    let raw_before = context.user_context.to_json_string().unwrap();
    assert_eq!(
        context
            .user_context
            .object_entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.code_units.to_vec())
            .collect::<Vec<_>>(),
        expected_source_keys
    );

    let entries = context.user_context_entries();
    assert_eq!(
        entries
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>(),
        vec![
            "0".encode_utf16().collect::<Vec<_>>(),
            "2".encode_utf16().collect(),
            "10".encode_utf16().collect(),
            "4294967294".encode_utf16().collect(),
            "before".encode_utf16().collect(),
            "01".encode_utf16().collect(),
            "4294967295".encode_utf16().collect(),
            vec![0xd800],
        ]
    );
    assert_eq!(context.user_context.to_json_string().unwrap(), raw_before);

    let reminder_units = context.reminder_source().unwrap().string_units("").unwrap();
    let labels = [
        "# 0\nzero",
        "# 2\ntwo",
        "# 10\nten",
        "# 4294967294\nlast-array-index",
        "# before\nbefore",
        "# 01\nzero-padded",
        "# 4294967295\nnot-an-index",
    ];
    let mut prior_end = 0;
    for label in labels {
        let label = label.encode_utf16().collect::<Vec<_>>();
        let start = reminder_units[prior_end..]
            .windows(label.len())
            .position(|window| window == label.as_slice())
            .map(|offset| prior_end + offset)
            .expect("reminder follows ECMAScript key enumeration");
        prior_end = start + label.len();
    }
    assert!(reminder_units.windows(1).any(|window| window == [0xd800]));
}

#[test]
fn automem_renderer_applies_exclusions_and_tag_sanitizing_to_utf16_units() {
    let content_units = [
        0xd800,
        b' ' as u16,
        b'<' as u16,
        's' as u16,
        'y' as u16,
        's' as u16,
        't' as u16,
        'e' as u16,
        'm' as u16,
        b'-' as u16,
        'r' as u16,
        'e' as u16,
        'm' as u16,
        'i' as u16,
        'n' as u16,
        'd' as u16,
        'e' as u16,
        'r' as u16,
        b'>' as u16,
        b' ' as u16,
        0xdc00,
    ];
    let attachment = instructions_delta(
        &[InstructionFile::new(
            "/memory",
            InstructionFileType::AutoMem,
            Utf16JsonProjection::root_string(
                String::from_utf16_lossy(&content_units),
                content_units.to_vec(),
            )
            .unwrap(),
        )],
        None,
        None,
    )
    .unwrap();
    let rendered = render_instruction_attachment(&attachment).unwrap();
    let units = rendered.string_units("").unwrap();
    assert_eq!(units.first(), Some(&(b'<' as u16)));
    assert!(!units.contains(&0xd800));
    assert!(!units.contains(&0xdc00));
    assert!(units
        .windows(3)
        .any(|window| window == [b'<' as u16, b'\\' as u16, b's' as u16]));
}

#[test]
fn session_context_attachment_preserves_lone_utf16_value_through_rendering() {
    let mut context = InstructionContext::default();
    context
        .set_user_context_text(
            "userEmail",
            Utf16JsonProjection::root_string(
                String::from_utf16_lossy(&[b'a' as u16, 0xd800, b'b' as u16]),
                vec![b'a' as u16, 0xd800, b'b' as u16],
            )
            .unwrap(),
        )
        .unwrap();
    let attachments = context_attachments(&context, "2026-10-13", "session_start");
    let session_context = attachments
        .iter()
        .find(|attachment| attachment.string_at("/type") == Some("session_context"))
        .expect("eligible context field is announced");
    assert!(is_valid_context_attachment(session_context));
    let rendered = render_instruction_attachment(session_context).unwrap();
    assert!(rendered
        .string_units("")
        .unwrap()
        .windows(3)
        .any(|units| units == [b'a' as u16, 0xd800, b'b' as u16]));
}
