use super::parse_generated_session_name;

#[test]
fn parses_plain_and_fenced_json_but_rejects_empty_or_prose() {
    assert_eq!(
        parse_generated_session_name(&r#"{"name":"fix-login-bug"}"#.into())
            .and_then(|name| name.string_at("").map(str::to_owned))
            .as_deref(),
        Some("fix-login-bug")
    );
    assert_eq!(
        parse_generated_session_name(&"```json\n{\"name\":\"add-auth-feature\"}\n```".into())
            .and_then(|name| name.string_at("").map(str::to_owned))
            .as_deref(),
        Some("add-auth-feature")
    );
    assert_eq!(
        parse_generated_session_name(&r#"{"name":"  "}"#.into()),
        None
    );
    assert_eq!(parse_generated_session_name(&"not json".into()), None);
}

#[test]
fn keeps_unpaired_name_units_from_source_json() {
    let raw: lingxi_core::types::utf16_json::Utf16JsonProjection =
        r#"{"name":"title \ud800 tail"}"#.into();
    let parsed = parse_generated_session_name(&raw).expect("name is present");
    assert_eq!(
        parsed.string_units(""),
        Some(
            "title "
                .encode_utf16()
                .chain([0xd800])
                .chain(" tail".encode_utf16())
                .collect()
        )
    );
}

#[test]
fn trims_ecmascript_whitespace_and_keeps_non_js_space_units() {
    let raw: lingxi_core::types::utf16_json::Utf16JsonProjection =
        r#"{"name":"\ufeff\u2000title\u0085"}"#.into();
    let parsed = parse_generated_session_name(&raw).expect("name is present");
    assert_eq!(
        parsed.string_units(""),
        Some("title\u{0085}".encode_utf16().collect())
    );
}
