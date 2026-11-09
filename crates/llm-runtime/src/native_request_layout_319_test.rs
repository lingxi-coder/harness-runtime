//! Current-release byte locks at the runtime's SDK policy boundary.
use lingxi_llm_client::exact_json::{serialize_json_payload_with_overrides, JsonEncoding};
use lingxi_llm_client::protocol::JsonPayload;
use lingxi_llm_client::providers::anthropic::request_policy::{
    AnthropicRequestKind, AnthropicRequestPolicy,
};
use std::collections::BTreeMap;

#[test]
fn main_and_side_requests_match_native_319_template_bytes() {
    let fixture = JsonPayload::parse(include_str!(
        "../tests/fixtures/request_layout_2_1_319.json"
    ))
    .unwrap();
    assert_eq!(fixture.string_at("/version").as_deref(), Some("2.1.319"));
    let cases = fixture.payload_at("/cases").unwrap();
    for index in 0..cases.array_len("").unwrap() {
        let case = cases.array_item(index).unwrap();
        let mut body = case.payload_at("/input/base").unwrap();
        let request_kind = match case.string_at("/input/kind").as_deref() {
            Some("main") => AnthropicRequestKind::Main,
            Some("side") => AnthropicRequestKind::SideQuery,
            other => panic!("unexpected request kind: {other:?}"),
        };
        let mut overrides = BTreeMap::new();
        let extra_body = case.payload_at("/input/extra").unwrap();
        crate::native_request_layout::reserve_native_request_properties(
            &mut body,
            &extra_body,
            request_kind,
        )
        .unwrap();
        AnthropicRequestPolicy {
            request_kind,
            extra_body,
            ..Default::default()
        }
        .apply(
            &mut body,
            &mut BTreeMap::new(),
            &mut overrides,
            &mut "https://api.anthropic.com/v1/messages".to_owned(),
        )
        .unwrap();
        let actual =
            serialize_json_payload_with_overrides(&body, &overrides, JsonEncoding::JavaScript)
                .unwrap();
        let expected = case.string_at("/expected/body_json").unwrap();
        assert_eq!(actual, expected.as_bytes(), "case {index}");
    }
}
