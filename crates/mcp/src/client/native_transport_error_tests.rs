use super::*;

fn native_failure(
    error: &jsonrpc::ConnectionError,
    server: &str,
    tool: &str,
) -> Option<McpClientError> {
    super::native_transport_tool_error(error, &server.into(), &tool.into())
}

fn remote_failure(code: i64, data: &str) -> jsonrpc::ConnectionError {
    jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(jsonrpc::ResponseError {
        code,
        message: "local transport failure".into(),
        data: Some(jsonrpc::JsonPayload::parse(data).unwrap()),
    }))
}

#[test]
fn native_transport_tool_diagnostics_match_captured_cli_text_without_rpc_prefix() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/native_transport_failure_2_1_322.json"
    ))
    .unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let data = serde_json::json!({
            "nativeFailure":case["kind"],"diagnostic":case["diagnostic"]
        });
        let error = native_failure(
            &remote_failure(-32_001, &data.to_string()),
            case["server"].as_str().unwrap(),
            case["tool"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            error.source_diagnostic().unwrap().string_at(""),
            case["expected"].as_str(),
            "{}",
            case["name"]
        );
        assert!(!error.is_auth_response());
        assert!(
            !error.is_session_expired(),
            "an uncertain tool must not be retried"
        );
    }
}

#[test]
fn native_schema_diagnostics_retain_unpaired_utf16_source_units() {
    let error = native_failure(
        &remote_failure(
            -32_001,
            r#"{"nativeFailure":"schema","diagnostic":"issue-\ud800Ω"}"#,
        ),
        "wire audit",
        "echo",
    )
    .unwrap();
    let mut expected = "MCP server \"wire audit\" returned a malformed result that failed schema validation: issue-"
        .encode_utf16()
        .collect::<Vec<_>>();
    expected.extend([0xd800, 0x03a9]);
    assert_eq!(
        error.source_diagnostic().unwrap().string_units(""),
        Some(expected)
    );
}

#[test]
fn native_transport_diagnostic_preserves_auth_and_session_recovery_metadata() {
    for status in [400, 401, 403, 404, 500] {
        let data = serde_json::json!({"nativeFailure":"transport",
            "diagnostic":"Error POSTing to endpoint: fixture Ω", "httpStatus":status,
            "wwwAuthenticate":"Bearer resource_metadata=\"https://example.test/prm\""});
        let error = native_failure(
            &remote_failure(-32_001, &data.to_string()),
            "server",
            "tool",
        )
        .unwrap();
        assert_eq!(
            error.source_diagnostic().unwrap().string_at(""),
            Some("Error POSTing to endpoint: fixture Ω")
        );
        assert!(
            matches!(&error, McpClientError::HttpResponse { status: actual, .. } if *actual == status)
        );
        assert_eq!(error.is_auth_response(), matches!(status, 401 | 403));
        assert_eq!(error.is_session_expired(), status == 404);
    }
}

#[test]
fn contextual_transport_warnings_keep_exact_authored_server_and_tool_names() {
    let server = JsonProjection::parse(r#""server-\ud800""#).unwrap();
    let tool = JsonProjection::parse(r#""tool-\udfff""#).unwrap();
    for kind in ["syntax", "schema", "body-overflow"] {
        let data = serde_json::json!({"nativeFailure":kind,"diagnostic":"detail"});
        let error = super::native_transport_tool_error(
            &remote_failure(-32_001, &data.to_string()),
            &server,
            &tool,
        )
        .unwrap();
        let units = error.source_diagnostic().unwrap().string_units("").unwrap();
        assert!(units.contains(&0xd800));
        assert_eq!(units.contains(&0xdfff), kind != "schema");
        assert!(!error.is_auth_response());
        assert!(!error.is_session_expired());
    }
}

#[test]
fn unknown_or_invalid_transport_markers_keep_the_existing_rpc_error_contract() {
    for data in [
        "{}",
        r#"{"nativeFailure":"other","diagnostic":"detail"}"#,
        r#"{"nativeFailure":"schema","diagnostic":17}"#,
        r#"{"nativeFailure":"syntax"}"#,
    ] {
        assert!(native_failure(&remote_failure(-32_001, data), "server", "tool").is_none());
    }
    assert!(native_failure(
        &remote_failure(
            -32_000,
            r#"{"nativeFailure":"syntax","diagnostic":"detail"}"#
        ),
        "server",
        "tool"
    )
    .is_none());
}
