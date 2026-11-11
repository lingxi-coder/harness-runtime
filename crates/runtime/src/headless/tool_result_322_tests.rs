use super::*;

#[tokio::test]
async fn native_no_approval_diagnostic_uses_owned_paths_without_changing_authorization() {
    use lingxi_core::host::permission_gate::{PermissionCheckContext, PermissionGate};
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let roots = permission::filesystem::FsRoots {
        cwd: cwd.clone(),
        home: None,
        lingxi_home: cwd.join("state"),
    };
    let policy = Arc::new(
        permission::PermissionPolicy::new(permission::PermissionMode::Default).with_roots(roots),
    );
    let gate = permission::PolicyPermissionGate::new(policy, Arc::new(permission::DenyOnAskGate));
    let input = Utf16JsonProjection::parse(r#"{"command":"printf marker > out.txt"}"#).unwrap();
    let ctx = PermissionCheckContext::default();
    let before = gate.check_with_context("Bash", &input, &ctx).await;
    assert!(!gate.has_approval_surface());
    let message = gate
        .native_no_approval_diagnostic(
            "Bash",
            &input,
            &ctx,
            Some(&Utf16JsonProjection::from("This command requires approval")),
        )
        .unwrap();
    assert!(message.string_at("").unwrap().ends_with(&format!(
        "What required approval: Output redirection to '{}' needs approval. The path is inside the working directories for this session ('{}'), and Claude Code asks before a shell command creates, changes or removes files there.",
        cwd.join("out.txt").display(), cwd.display())));
    let protected = Utf16JsonProjection::from("Protected path needs explicit approval");
    let specific = gate
        .native_no_approval_diagnostic("Bash", &input, &ctx, Some(&protected))
        .unwrap();
    assert!(specific
        .string_at("")
        .unwrap()
        .ends_with("What required approval: Protected path needs explicit approval"));
    let hook_ctx = PermissionCheckContext {
        decision_reason_type: Some("hook".into()),
        decision_reason: Some(Utf16JsonProjection::from("The hook requires approval")),
        ..Default::default()
    };
    let hook = gate
        .native_no_approval_diagnostic(
            "Bash",
            &input,
            &hook_ctx,
            Some(&Utf16JsonProjection::from("This command requires approval")),
        )
        .unwrap();
    assert!(hook
        .string_at("")
        .unwrap()
        .ends_with("What required approval: The hook requires approval"));
    assert_eq!(gate.check_with_context("Bash", &input, &ctx).await, before);
    assert!(matches!(
        before,
        lingxi_core::host::permission_gate::PermissionOutcome::Deny { .. }
    ));
}

#[test]
fn native_no_approval_diagnostic_preserves_exact_utf16_ask() {
    let ask = Utf16JsonProjection::parse(r#""unavailable \ud800 action""#).unwrap();
    let message = permission::headless_gate::native_no_approval_message(Some(&ask));
    let units = message.string_units("").unwrap();
    let mut expected = " What required approval: "
        .encode_utf16()
        .collect::<Vec<_>>();
    expected.extend(ask.string_units("").unwrap());
    assert!(units.ends_with(&expected));
    assert!(message.string_at("").unwrap().starts_with("Permission for this tool use was denied. It requires approval, and this session has no approval surface"));
}

#[tokio::test]
async fn structured_tool_keeps_native_error_prefix_and_exact_utf16_detail_once() {
    use lingxi_core::host::json_schema_validator::{
        CompiledJsonSchema, JsonSchemaValidation, SchemaWorkerError,
    };
    use tool_api::{Tool, ToolUseContext};
    struct InvalidPlan(Utf16JsonProjection);
    #[async_trait]
    impl CompiledJsonSchema for InvalidPlan {
        async fn validate(
            &self,
            _: &Utf16JsonProjection,
        ) -> Result<JsonSchemaValidation, SchemaWorkerError> {
            Ok(JsonSchemaValidation::Invalid {
                detail: self.0.clone(),
                keywords: vec!["type".into()],
            })
        }
    }
    let expected = Utf16JsonProjection::parse(
        r#""Output does not match required schema: invalid \ud800 output""#,
    )
    .unwrap();
    for detail in [
        Utf16JsonProjection::parse(r#""invalid \ud800 output""#).unwrap(),
        expected.clone(),
    ] {
        let previous = Utf16JsonProjection::parse(r#"{"answer":"accepted earlier"}"#).unwrap();
        let slot = Arc::new(StdMutex::new(Some(previous.clone())));
        let tool = orchestrator::structured_output::StructuredOutputTool::new(
            Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
            Arc::new(InvalidPlan(detail)),
            slot.clone(),
        );
        let (progress, _) = tool_api::progress::progress_channel();
        let error = tool
            .call(
                Utf16JsonProjection::parse("{}").unwrap(),
                ToolUseContext::model_seed("test".into()),
                progress,
            )
            .await
            .unwrap_err();
        assert_eq!(error.model_facing_message(), expected);
        assert_eq!(slot.lock().unwrap().as_ref(), Some(&previous));
    }
}

#[tokio::test]
async fn denied_host_result_preserves_source_and_native_error_layout() {
    let bytes = Arc::new(StdMutex::new(Vec::new()));
    let stream = StreamJsonStream::new(
        super::tests::make_params("session"),
        Output::new(super::verbose_json_tests::CapturedWriter(bytes.clone())),
    );
    let text = Utf16JsonProjection::parse(r#""denied \ud800""#).unwrap();
    let result = Utf16JsonProjection::parse(r#"{"error":"denied \ud800"}"#).unwrap();
    stream
        .emit_tool_result_with_metadata(
            &lingxi_core::types::ToolUseId::from("tool_denied"),
            "Bash",
            &text,
            &result,
            None,
            Some("permission-rule"),
            &lingxi_core::host::orchestrator::ToolResultFrameMetadata {
                permission_decision: Some(project_json_value(
                    &json!({"decision":"reject","source":"user_reject"}),
                )),
                ..Default::default()
            },
        )
        .await;
    stream.finish().await.unwrap();
    let raw = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(raw.contains(r#""message":{"role":"user","content":[{"type":"tool_result","content":"denied \ud800","is_error":true,"tool_use_id":"tool_denied"}]}"#));
    assert!(raw.contains(r#""tool_use_result":"Error: denied \ud800""#));
    assert!(raw.contains(r#""tool_result_meta":[{"id":"tool_denied","non_execution_kind":"permission-rule","permission_decision":{"decision":"reject","source":"user_reject"}}]"#));
}

#[tokio::test]
async fn accepted_permission_fact_uses_native_322_tool_result_envelope() {
    let bytes = Arc::new(StdMutex::new(Vec::new()));
    let stream = StreamJsonStream::new(
        super::tests::make_params("session"),
        Output::new(super::verbose_json_tests::CapturedWriter(bytes.clone())),
    );
    let id = lingxi_core::types::ToolUseId::from("tool_headless_1");
    let data = project_json_value(
        &json!({"stdout":"HEADLESS_TOOL","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false}),
    );
    let permission =
        project_json_value(&json!({"decision":"accept","source":"config","reason_type":"rule"}));
    stream
        .emit_tool_result_with_metadata(
            &id,
            "Bash",
            &Utf16JsonProjection::from("HEADLESS_TOOL"),
            &data,
            None,
            None,
            &lingxi_core::host::orchestrator::ToolResultFrameMetadata {
                row_id: Some(lingxi_core::types::MessageId::nil()),
                timestamp: Some("2026-11-10T00:00:00.000Z".into()),
                permission_decision: Some(permission),
            },
        )
        .await;
    stream.finish().await.unwrap();
    let raw = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    let keys = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "type",
            "message",
            "parent_tool_use_id",
            "session_id",
            "uuid",
            "timestamp",
            "tool_use_result",
            "tool_result_meta"
        ]
    );
    assert!(raw.contains(r#""message":{"role":"user","content":[{"tool_use_id":"tool_headless_1","type":"tool_result","content":"HEADLESS_TOOL","is_error":false}]}"#));
    assert_eq!(
        value["tool_result_meta"],
        json!([{"id":"tool_headless_1","permission_decision":{"decision":"accept","source":"config","reason_type":"rule"}}])
    );
    assert_eq!(value["uuid"], uuid::Uuid::nil().to_string());
    assert_eq!(value["timestamp"], "2026-11-10T00:00:00.000Z");
    assert!(value.get("toolUseResult").is_none());
}
