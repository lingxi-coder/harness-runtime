//! Differential memory storage vectors evaluated from the pinned native release.
use lingxi_core::host::instruction_memory_sanitize::sanitize_memory_body;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use memory::{
    auto_memory_enabled, memory_index_cap_notice_measured, truncate_memory_index, AutoMemoryEnv,
};

fn fixture() -> Utf16JsonProjection {
    Utf16JsonProjection::parse(include_str!("fixtures/memory_storage_2_1_319.json"))
        .expect("native fixture includes exact UTF-16 strings")
}

fn cases<'a>(fixture: &'a Utf16JsonProjection, name: &str) -> Vec<JsonValueView<'a>> {
    let rows = fixture.get_property(name).unwrap();
    (0..rows.array_len().unwrap())
        .map(|index| rows.get_index(index).unwrap())
        .collect()
}

fn text<'a>(row: JsonValueView<'a>, field: &str) -> &'a str {
    row.get_property(field).unwrap().as_str().unwrap()
}

#[test]
fn index_projection_matches_native_units_and_warnings() {
    let fixture = fixture();
    for case in cases(&fixture, "index_cases") {
        let input = text(case, "input");
        let actual = truncate_memory_index(input);
        let measured = memory::measure(input);
        let expected = case.get_property("expected").unwrap();
        let name = text(case, "name");
        assert_eq!(
            measured.byte_count, actual.byte_count,
            "{name}: measurement units"
        );
        assert_eq!(
            measured.line_count, actual.line_count,
            "{name}: measurement lines"
        );
        assert_eq!(
            actual.content.string_units("").unwrap().as_slice(),
            expected
                .get_property("content")
                .unwrap()
                .code_units()
                .unwrap(),
            "{name}: content"
        );
        for (key, actual) in [
            ("lineCount", actual.line_count),
            ("byteCount", actual.byte_count),
        ] {
            assert_eq!(
                actual as f64,
                expected.get_property(key).unwrap().as_number().unwrap(),
                "{name}: {key}"
            );
        }
        for (key, actual) in [
            ("wasLineTruncated", actual.was_line_truncated),
            ("wasByteTruncated", actual.was_byte_truncated),
        ] {
            assert_eq!(
                actual,
                expected.get_property(key).unwrap().as_bool().unwrap(),
                "{name}: {key}"
            );
        }
    }
}

#[test]
fn index_write_advisory_matches_native_worst_dimension() {
    let fixture = fixture();
    for case in cases(&fixture, "advisory_cases") {
        let bytes = case.get_property("bytes").unwrap().as_number().unwrap() as usize;
        let lines = case.get_property("lines").unwrap().as_number().unwrap() as usize;
        let actual = memory_index_cap_notice_measured(bytes, lines);
        let expected = case.get_property("expected").unwrap();
        if expected.is_null() {
            assert!(actual.is_none(), "{bytes} units/{lines} lines");
        } else {
            let actual = actual.expect("native advisory present");
            assert_eq!(
                actual.text,
                text(expected, "text"),
                "{bytes} units/{lines} lines"
            );
            assert_eq!(
                actual.over_cap,
                expected.get_property("overCap").unwrap().as_bool().unwrap()
            );
        }
    }
    for case in cases(&fixture, "byte_format_cases") {
        let bytes = case.get_property("bytes").unwrap().as_number().unwrap() as usize;
        assert_eq!(
            memory::human_bytes(bytes),
            text(case, "expected"),
            "{bytes} bytes"
        );
    }
}

#[test]
fn explicit_gate_values_match_native_order_and_js_whitespace() {
    let fixture = fixture();
    for case in cases(&fixture, "gate_cases") {
        let env = AutoMemoryEnv {
            disable_auto_memory: case
                .get_property("disable")
                .unwrap()
                .as_str()
                .map(str::to_owned),
            simple_mode: case.get_property("simple").unwrap().as_bool().unwrap(),
        };
        let setting = case.get_property("setting").unwrap().as_bool();
        assert_eq!(
            auto_memory_enabled(&env, setting),
            case.get_property("expected").unwrap().as_bool().unwrap(),
            "{env:?}, setting {setting:?}"
        );
    }
}

#[test]
fn memory_sanitation_matches_native_tags_and_unicode() {
    let fixture = fixture();
    for case in cases(&fixture, "sanitize_cases") {
        let input = text(case, "input");
        let expected = text(case, "expected");
        assert_eq!(sanitize_memory_body(input), expected, "{input:?}");
        assert_eq!(
            lingxi_core::host::instruction_memory_sanitize::sanitize_memory_body_utf16(
                &input.encode_utf16().collect::<Vec<_>>()
            ),
            expected.encode_utf16().collect::<Vec<_>>(),
            "UTF16 {input:?}"
        );
    }
}

#[test]
fn index_is_spliced_before_native_sanitization() {
    let fixture = fixture();
    for case in cases(&fixture, "loaded_index_cases") {
        let actual = memory::prefetch::load_memory_index(text(case, "input"));
        assert_eq!(
            actual.content.string_units("").unwrap().as_slice(),
            case.get_property("expected").unwrap().code_units().unwrap(),
            "{}",
            text(case, "name")
        );
    }
}

#[test]
fn index_frontmatter_matches_native_v9o_bom_and_loose_delimiters() {
    let fixture = fixture();
    for case in cases(&fixture, "index_frontmatter_cases") {
        let input = text(case, "input");
        assert_eq!(
            memory::lingxi_md::loader::strip_frontmatter(input),
            text(case, "expected_body"),
            "{}: frontmatter body",
            text(case, "name")
        );
        let actual = memory::prefetch::load_memory_index(input);
        assert_eq!(
            actual.content.string_units("").unwrap().as_slice(),
            case.get_property("expected").unwrap().code_units().unwrap(),
            "{}",
            text(case, "name")
        );
    }
}

#[test]
#[cfg(unix)]
fn configured_memory_directory_matches_native_path_validation() {
    let fixture = fixture();
    for case in cases(&fixture, "path_cases") {
        let input = case.get_property("input").unwrap().as_str();
        let actual = memory::memdir::paths::validated_auto_memory_directory(
            std::path::Path::new("/home/user"),
            input,
        );
        let expected = case.get_property("expected").unwrap().as_str();
        assert_eq!(
            actual.as_deref(),
            expected.map(std::path::Path::new),
            "{input:?}"
        );
    }
}
