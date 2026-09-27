//! `extract_json_string_field` + `unescape_json_string` parity with
//! sessionStoragePortable.ts:39-46, 53-76.

use session::jsonl::reader::{extract_json_string_field, unescape_json_string};

#[test]
fn finds_basic_key_value_pair() {
    let text = r#"{"sessionId":"abc-123","cwd":"/x"}"#;
    assert_eq!(
        extract_json_string_field(text, "sessionId"),
        Some("abc-123".to_string())
    );
}

#[test]
fn handles_optional_space_after_colon() {
    let text = r#"{"sessionId": "abc-123"}"#;
    assert_eq!(
        extract_json_string_field(text, "sessionId"),
        Some("abc-123".to_string())
    );
}

#[test]
fn returns_none_when_key_missing() {
    let text = r#"{"cwd":"/x"}"#;
    assert_eq!(extract_json_string_field(text, "sessionId"), None);
}

#[test]
fn first_match_wins_when_key_appears_twice() {
    // The pattern scan is left-to-right and returns the FIRST hit.
    let text = r#"{"k":"first","k":"second"}"#;
    assert_eq!(
        extract_json_string_field(text, "k"),
        Some("first".to_string())
    );
}

#[test]
fn backslash_escapes_next_char_in_value() {
    // The value `a\"b` (literal a, escaped quote, literal b) is the JSON
    // string `a"b` after unescape.
    let text = r#"{"k":"a\"b"}"#;
    assert_eq!(
        extract_json_string_field(text, "k"),
        Some("a\"b".to_string())
    );
}

#[test]
fn unescape_passthrough_when_no_backslash() {
    assert_eq!(unescape_json_string("hello world"), "hello world");
}

#[test]
fn unescape_decodes_common_sequences() {
    assert_eq!(unescape_json_string(r"a\nb"), "a\nb");
    assert_eq!(unescape_json_string(r#"a\"b"#), "a\"b");
    assert_eq!(unescape_json_string(r"a\\b"), "a\\b");
}
