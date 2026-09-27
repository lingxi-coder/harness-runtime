use super::parse_generated_session_name;

#[test]
fn parses_plain_and_fenced_json_but_rejects_empty_or_prose() {
    assert_eq!(
        parse_generated_session_name(r#"{"name":"fix-login-bug"}"#).as_deref(),
        Some("fix-login-bug")
    );
    assert_eq!(
        parse_generated_session_name("```json\n{\"name\":\"add-auth-feature\"}\n```").as_deref(),
        Some("add-auth-feature")
    );
    assert_eq!(parse_generated_session_name(r#"{"name":"  "}"#), None);
    assert_eq!(parse_generated_session_name("not json"), None);
}
