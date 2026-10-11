//! Replay of fixed Native 2.1.295 `roots/list` input-required frames.
use async_trait::async_trait;
use lingxi_core::host::mcp_result::{
    drive_modern_request, validate_embedded_request, validate_input_request_schema,
    CancellationToken, McpInputRequiredIo, McpInputRequiredOptions, McpResultError,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::{collections::VecDeque, sync::Mutex};

fn projection(raw: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(raw).expect("captured JSON-RPC frame is valid JSON")
}

fn field(value: &Utf16JsonProjection, pointer: &str) -> Utf16JsonProjection {
    value
        .subprojection(pointer)
        .unwrap_or_else(|error| panic!("captured field {pointer} is present: {error}"))
}

struct CapturedReplayIo {
    replies: Mutex<VecDeque<Utf16JsonProjection>>,
    roots_response: Utf16JsonProjection,
    calls: Mutex<Vec<(String, Utf16JsonProjection)>>,
    dispatches: Mutex<Vec<(Vec<u16>, Utf16JsonProjection, Option<String>)>>,
}

#[async_trait]
impl McpInputRequiredIo for CapturedReplayIo {
    async fn request(
        &self,
        method: &str,
        params: Utf16JsonProjection,
        _options: &McpInputRequiredOptions,
    ) -> Result<Utf16JsonProjection, McpResultError> {
        self.calls.lock().unwrap().push((method.to_owned(), params));
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("one captured input_required result and one completion"))
    }

    async fn dispatch_local(
        &self,
        key_units: Vec<u16>,
        request: Utf16JsonProjection,
        _cancellation: CancellationToken,
    ) -> Result<Utf16JsonProjection, McpResultError> {
        let projected = validate_embedded_request(&key_units, &request)?;
        let projected_params = projected.params.map(|payload| payload.into_json());
        self.dispatches
            .lock()
            .unwrap()
            .push((key_units, request, projected_params));
        Ok(self.roots_response.clone())
    }
}

async fn replay_case(
    initial_call: &str,
    input_required: &str,
    followup_call: &str,
    complete: &str,
    case_name: &str,
    expected_source_params: Option<&str>,
) {
    let initial_frame = projection(initial_call);
    let input_required_frame = projection(input_required);
    let followup_frame = projection(followup_call);
    let complete_frame = projection(complete);

    let embedded = field(&input_required_frame, "/result/inputRequests/r");
    // This replay tests the captured local-dispatch path, not the exported
    // InputRequestSchema predicate. Native 2.1.295 dispatch accepts these
    // roots/list source parameters even though the schema stays stricter.
    assert_eq!(
        embedded.string_at("/method"),
        Some("roots/list"),
        "{case_name}"
    );
    if let Some(expected) = expected_source_params {
        assert_eq!(
            embedded
                .subprojection("/params")
                .unwrap()
                .to_json_string()
                .unwrap(),
            expected
        );
    } else {
        assert_eq!(embedded.string_at("/params"), Some("bad"), "{case_name}");
    }

    let native_followup_params = field(&followup_frame, "/params");
    let native_input_response = field(&native_followup_params, "/inputResponses/r");
    assert_eq!(
        native_input_response.array_len("/roots"),
        Some(1),
        "{case_name}"
    );
    let context: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/native_mcp_roots_295/native_context.json"
    ))
    .expect("captured cwd metadata is valid JSON");
    let native_cwd = context["cases"][case_name]["native_cwd"]
        .as_str()
        .expect("the capture records this case's cwd");
    let expected_uri = format!("file://{native_cwd}");
    assert_eq!(
        native_input_response.string_at("/roots/0/uri"),
        Some(expected_uri.as_str()),
        "the captured filesystem URI is constrained to this case's native cwd; it is not cross-checkout byte parity"
    );

    // This replay supplies the raw captured response as the local-dispatch
    // result. It verifies forwarding through the driver, not roots enumeration.
    let io = CapturedReplayIo {
        replies: Mutex::new(VecDeque::from([
            field(&input_required_frame, "/result"),
            field(&complete_frame, "/result"),
        ])),
        roots_response: native_input_response.clone(),
        calls: Mutex::new(Vec::new()),
        dispatches: Mutex::new(Vec::new()),
    };
    let original = field(&initial_frame, "/params");
    assert_eq!(initial_frame.string_at("/method"), Some("tools/call"));
    assert!(!original.contains_property("inputResponses"));
    let result = drive_modern_request(
        &io,
        "tools/call",
        original.clone(),
        McpInputRequiredOptions::default(),
    )
    .await
    .unwrap_or_else(|error| panic!("captured Native case {case_name} must replay: {error}"));

    assert_eq!(
        result.string_at("/content/0/text"),
        complete_frame.string_at("/result/content/0/text")
    );
    let calls = io.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "{case_name}");
    assert!(
        calls.iter().all(|(method, _)| method == "tools/call"),
        "{case_name}"
    );
    assert_eq!(calls[0].1, original, "initial call params are preserved");
    assert_eq!(
        field(&calls[1].1, "/inputResponses/r"),
        native_input_response,
        "the driver must forward the captured local-dispatch response into the followup"
    );
    assert_eq!(
        field(&calls[1].1, "/name"),
        field(&native_followup_params, "/name"),
        "the existing tool name survives answer replacement"
    );
    assert_eq!(
        field(&calls[1].1, "/arguments"),
        field(&native_followup_params, "/arguments"),
        "the existing tool arguments survive answer replacement"
    );

    let dispatches = io.dispatches.lock().unwrap();
    assert_eq!(dispatches.len(), 1, "{case_name}");
    assert_eq!(dispatches[0].0, "r".encode_utf16().collect::<Vec<_>>());
    assert_eq!(
        dispatches[0].1, embedded,
        "the input request reaches real embedded-request validation"
    );
    match expected_source_params {
        Some(expected) => assert_eq!(dispatches[0].2.as_deref(), Some(expected)),
        None => assert!(
            dispatches[0].2.is_none(),
            "non-object roots params are omitted from local dispatch"
        ),
    }
}

#[tokio::test]
async fn native_295_roots_list_replays_loose_params_and_forwards_captured_roots_response() {
    replay_case(
        include_str!("fixtures/native_mcp_roots_295/accepted/initial-tools-call.raw.jsonrpc"),
        include_str!("fixtures/native_mcp_roots_295/accepted/input-required.raw.jsonrpc"),
        include_str!("fixtures/native_mcp_roots_295/accepted/followup-call.raw.jsonrpc"),
        include_str!("fixtures/native_mcp_roots_295/accepted/complete.raw.jsonrpc"),
        "accepted_meta_string",
        Some(r#"{"extra":"keep","_meta":"not-object"}"#),
    )
    .await;
    replay_case(
        include_str!(
            "fixtures/native_mcp_roots_295/invalid-primitive/initial-tools-call.raw.jsonrpc"
        ),
        include_str!("fixtures/native_mcp_roots_295/invalid-primitive/input-required.raw.jsonrpc"),
        include_str!("fixtures/native_mcp_roots_295/invalid-primitive/followup-call.raw.jsonrpc"),
        include_str!("fixtures/native_mcp_roots_295/invalid-primitive/complete.raw.jsonrpc"),
        "invalid_params_string",
        None,
    )
    .await;
}

#[test]
fn roots_input_schema_is_strict_while_dispatch_projects_loose_params() {
    let primitive = projection(r#"{"method":"roots/list","params":"bad"}"#);
    assert!(!validate_input_request_schema(&primitive));
    let dispatched = validate_embedded_request(&[], &primitive)
        .expect("roots/list dispatch accepts and omits non-object params");
    assert!(dispatched.params.is_none());

    let malformed_meta =
        projection(r#"{"method":"roots/list","params":{"_meta":"not-object"}}"#);
    assert!(!validate_input_request_schema(&malformed_meta));
    let dispatched = validate_embedded_request(&[], &malformed_meta)
        .expect("roots/list dispatch retains loose source params");
    assert!(dispatched.params.is_some());
}

#[test]
fn non_roots_embedded_methods_still_require_object_params() {
    for method in ["elicitation/create", "sampling/createMessage"] {
        let request = projection(&format!(r#"{{"method":"{method}","params":"bad"}}"#));
        assert!(!validate_input_request_schema(&request), "{method}");
        assert!(matches!(
            validate_embedded_request(&[], &request),
            Err(McpResultError::Local(error)) if error.code == jsonrpc::INVALID_PARAMS
        ));
    }
}
