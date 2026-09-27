//! T2 tests — byte-lock `extract_title` against the cases in plan T0 step 3.

#![allow(clippy::needless_pass_by_value)]

use serde_json::json;
use session::jsonl::{extract_title, JsonlMessage};

fn make_user(content: serde_json::Value) -> JsonlMessage {
    serde_json::from_value(json!({
        "type": "user",
        "uuid": "11111111-1111-1111-1111-111111111111",
        "parentUuid": null,
        "sessionId": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": "/tmp",
        "version": "0.6.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "user", "content": content}
    }))
    .expect("user message")
}

#[test]
fn empty_messages_returns_session_fallback() {
    // SESSION.2: TS picker enrichLog sets firstPrompt='(session)' when the
    // derivation yields nothing (sessionStorage.ts:5052-5053).
    let messages: Vec<JsonlMessage> = vec![];
    assert_eq!(extract_title(&messages), "(session)");
}

#[test]
fn no_user_message_returns_session_fallback() {
    let mut m = make_user(json!("hi"));
    m.message_type = "assistant".to_string();
    assert_eq!(extract_title(&[m]), "(session)");
}

#[test]
fn empty_user_content_returns_session_fallback() {
    let m = make_user(json!(""));
    assert_eq!(extract_title(&[m]), "(session)");
}

#[test]
fn short_string_content_is_returned_verbatim() {
    let m = make_user(json!("hello world"));
    assert_eq!(extract_title(&[m]), "hello world");
}

#[test]
fn exactly_200_chars_is_returned_without_ellipsis() {
    // SESSION.2: TS caps firstPrompt at 200 chars (sessionStorage.ts:1732-1733).
    let s: String = "a".repeat(200);
    let m = make_user(json!(s.clone()));
    assert_eq!(extract_title(&[m]), s);
}

#[test]
fn two_hundred_one_chars_is_truncated_with_ellipsis() {
    let s: String = "a".repeat(201);
    let m = make_user(json!(s));
    let title = extract_title(&[m]);
    assert_eq!(title.chars().count(), 201); // 200 'a' + 1 ellipsis
    assert!(title.ends_with('…'));
    assert_eq!(title.chars().take(200).collect::<String>(), "a".repeat(200));
}

#[test]
fn short_cjk_is_returned_verbatim_under_200() {
    // 60 CJK chars = 180 UTF-8 bytes but only 60 chars < 200, so no truncation.
    let s: String = "中".repeat(60);
    let m = make_user(json!(s.clone()));
    assert_eq!(extract_title(&[m]), s);
}

#[test]
fn cjk_truncation_by_char_count_not_byte_count() {
    // 250 CJK chars > 200, so truncate to 200 chars + ellipsis (char count, not bytes).
    let s: String = "中".repeat(250);
    let m = make_user(json!(s));
    let title = extract_title(&[m]);
    assert_eq!(title.chars().count(), 201); // 200 '中' + '…'
    assert!(title.ends_with('…'));
}

#[test]
fn array_content_uses_first_text_block() {
    let m = make_user(json!([
        {"type": "text", "text": "first"},
        {"type": "text", "text": "second"}
    ]));
    assert_eq!(extract_title(&[m]), "first");
}

#[test]
fn array_content_with_only_image_returns_session_fallback() {
    let m = make_user(json!([
        {"type": "image", "source": {"type": "base64", "data": "xxxx"}}
    ]));
    assert_eq!(extract_title(&[m]), "(session)");
}

#[test]
fn whitespace_is_trimmed_before_truncation() {
    let m = make_user(json!("   hi   "));
    assert_eq!(extract_title(&[m]), "hi");
}

#[test]
fn skips_assistant_to_find_user() {
    let assistant = serde_json::from_value::<JsonlMessage>(json!({
        "type": "assistant",
        "uuid": "22222222-2222-2222-2222-222222222222",
        "parentUuid": null,
        "sessionId": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "timestamp": "2026-05-25T12:00:00.000Z",
        "cwd": "/tmp",
        "version": "0.6.0",
        "isSidechain": false,
        "userType": "external",
        "message": {"role": "assistant", "content": "I am the model"}
    }))
    .unwrap();
    let user = make_user(json!("real prompt"));
    assert_eq!(extract_title(&[assistant, user]), "real prompt");
}
