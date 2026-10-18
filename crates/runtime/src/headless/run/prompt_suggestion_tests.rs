use super::*;
use crate::headless::stream_json::OutboundMsg;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

#[tokio::test]
async fn sdk_suggestion_output_matches_native_line_separator_escaping() {
    use tokio::io::AsyncReadExt;
    let (output, mut reader) = tokio::io::duplex(4096);
    let stream = StreamJsonStream::new_placeholder(crate::headless::io::Output::new(output));
    let frame = Utf16JsonProjection::parse(
        r#"{"type":"prompt_suggestion","suggestion":"go\u2028now\u2029please","uuid":"frame-1","session_id":"session-fixture"}"#,
    )
    .unwrap();
    let expected = "{\"type\":\"prompt_suggestion\",\"suggestion\":\"go\\u2028now\\u2029please\",\"uuid\":\"frame-1\",\"session_id\":\"session-fixture\"}\n";
    stream.emit_prompt_suggestion_frame(&frame);
    stream.flush().await.unwrap();
    let mut bytes = vec![0; expected.len()];
    reader.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, expected.as_bytes());
    stream.finish().await.unwrap();
}

#[tokio::test]
async fn sdk_suggestion_output_preserves_lone_surrogate_as_json_string() {
    use tokio::io::AsyncReadExt;

    let frame = Utf16JsonProjection::parse(
        r#"{"type":"prompt_suggestion","suggestion":"try \ud800 now","uuid":"frame-1","session_id":"session-fixture"}"#,
    )
    .unwrap();
    let (output, mut reader) = tokio::io::duplex(4096);
    let stream = StreamJsonStream::new_placeholder(crate::headless::io::Output::new(output));
    let expected = r#"{"type":"prompt_suggestion","suggestion":"try \ud800 now","uuid":"frame-1","session_id":"session-fixture"}"#;

    stream.emit_prompt_suggestion_frame(&frame);
    stream.flush().await.unwrap();
    let mut bytes = vec![0; expected.len() + 1];
    reader.read_exact(&mut bytes).await.unwrap();
    let wire = String::from_utf8(bytes).unwrap();
    assert_eq!(wire, format!("{expected}\n"));
    assert!(!wire.contains(r#""suggestion":["#));
    let decoded = Utf16JsonProjection::parse(wire.trim_end()).unwrap();
    assert_eq!(
        decoded.string_units("/suggestion"),
        Some(
            "try "
                .encode_utf16()
                .chain([0xd800])
                .chain(" now".encode_utf16())
                .collect()
        )
    );
    stream.finish().await.unwrap();
}

/// Captured from the pinned native binary with control-only stdin, an isolated
/// config directory and a dummy provider key. There are no user/model turns.
#[tokio::test]
async fn sdk_suggestion_control_matches_native_pause_wire() {
    let root = tempfile::tempdir().unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let runtime = super::tests::fixture_runtime(stream, root.path()).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = Arc::new(tx);
    let writer = ControlPlaneWriter::new(tx.clone());
    let plane = StdioControlPlane::new(tx.clone());
    let lifecycle = crate::headless::queued_commands::QueueLifecycle::new(tx, "fixture".into());
    let owner = Arc::new(PrintAuxTaskGroup::default());
    let suggestions = SdkPromptSuggestions::new(false);
    let (cancel, _) = tokio::sync::watch::channel(false);
    let end = Arc::new(tokio::sync::Notify::new());
    let cases = [
        None,
        Some(json!("yes")),
        Some(Value::Null),
        Some(json!(true)),
        Some(json!(false)),
        Some(json!([])),
        Some(json!({})),
        Some(json!(0)),
    ];
    let mut wire = String::new();
    for (index, paused) in cases.into_iter().enumerate() {
        let mut request = json!({"subtype":"set_prompt_suggestions_paused"});
        if let Some(paused) = paused {
            request["paused"] = paused;
        }
        let id = format!("pause-{index}");
        let frame = Utf16JsonProjection::plain(
            json!({"type":"control_request","request_id":id,"request":request}),
        );
        dispatch_control_request(
            "set_prompt_suggestions_paused",
            &id,
            &frame,
            &writer,
            &cancel,
            &lifecycle,
            &runtime.orchestrator,
            &runtime.task_registry,
            &runtime.session_cwd,
            &plane,
            &end,
            &[],
            &[],
            &[],
            &json!({}),
            "off",
            None,
            &StreamFileSuggestionIndex::default(),
            &suggestions,
            runtime.services.as_ref(),
            &owner,
            &runtime.execution_errors,
        )
        .await
        .unwrap();
        let OutboundMsg::Line(line) = rx.recv().await.unwrap() else {
            panic!("expected control response");
        };
        wire.push_str(&line);
    }
    assert_eq!(
        wire,
        include_str!("../../../tests/fixtures/prompt_suggestion_pause_2_1_296.jsonl")
    );
    assert!(rx.try_recv().is_err());
}
