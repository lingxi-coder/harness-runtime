use super::super::{decode_mobile_bash_tool_result, execute_mobile_bash_tool, MobileBashExecution};
use async_trait::async_trait;
use lingxi_core::types::utf16_json::{Utf16JsonBuilder, Utf16JsonProjection};
use std::sync::Arc;
use tool_api::{
    CleanupUnverifiedReason, ReportedExitCode, ReportedShellPayload, ReportedShellStream,
    ToolCallResult,
};

struct MalformedShell(Utf16JsonProjection);

#[async_trait]
impl tool_api::Tool for MalformedShell {
    fn name(&self) -> &str {
        "Shell"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::LazyLock<Utf16JsonProjection> =
            std::sync::LazyLock::new(|| Utf16JsonProjection::parse("{}").unwrap());
        &SCHEMA
    }
    fn input_repair_metadata(&self) -> tool_api::ToolInputRepairMetadata {
        tool_api::ToolInputRepairMetadata::Known {
            schema: tool_api::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::MAX_UNVERIFIED_OUTPUT_CHARS
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    async fn validate_input(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::ToolUseContext,
    ) -> Result<(), tool_api::ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        panic!("direct Bash does not enter the model dispatcher")
    }
    async fn description(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::DescriptionOptions,
    ) -> String {
        "test".into()
    }
    async fn prompt(&self, _: &tool_api::PromptOptions) -> Utf16JsonProjection {
        "test".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<ToolCallResult, tool_api::ToolError> {
        Ok(ToolCallResult::from_data(self.0.clone()))
    }
}

fn decode(
    payload: Utf16JsonProjection,
    is_error: bool,
) -> Result<MobileBashExecution, CleanupUnverifiedReason> {
    let mut result = ToolCallResult::from_data(payload);
    result.is_error = is_error;
    decode_mobile_bash_tool_result(result)
}

fn incomplete(
    result: Result<MobileBashExecution, CleanupUnverifiedReason>,
) -> Arc<ReportedShellPayload> {
    match result {
        Err(CleanupUnverifiedReason::ReportedPayload(payload)) => payload,
        _ => panic!("expected an incomplete reported shell payload"),
    }
}

fn number_payload(number: f64) -> Utf16JsonProjection {
    let mut builder = Utf16JsonBuilder::new();
    let exit = builder.push_number(number);
    let stdout = builder.push_string("stdout");
    let stderr = builder.push_string("stderr");
    let root = builder
        .push_object(vec![
            ("exit_code".into(), exit),
            ("stdout".into(), stdout),
            ("stderr".into(), stderr),
        ])
        .unwrap();
    builder.finish(root).unwrap()
}

#[test]
fn malformed_exit_is_not_synthesized_from_tool_error_flag() {
    for (json, expected) in [
        (
            r#"{"stdout":"out","stderr":"err"}"#,
            ReportedExitCode::Missing,
        ),
        (
            r#"{"exit_code":null,"stdout":"out","stderr":"err"}"#,
            ReportedExitCode::Null,
        ),
        (
            r#"{"exit_code":"0","stdout":"out","stderr":"err"}"#,
            ReportedExitCode::InvalidType,
        ),
        (
            r#"{"exit_code":0.5,"stdout":"out","stderr":"err"}"#,
            ReportedExitCode::NonInteger {
                bits: 0.5f64.to_bits(),
            },
        ),
        (
            r#"{"exit_code":2147483648,"stdout":"out","stderr":"err"}"#,
            ReportedExitCode::OutOfRange {
                bits: 2_147_483_648f64.to_bits(),
            },
        ),
        (
            r#"{"exit_code":-2147483649,"stdout":"out","stderr":"err"}"#,
            ReportedExitCode::OutOfRange {
                bits: (-2_147_483_649f64).to_bits(),
            },
        ),
    ] {
        for is_error in [false, true] {
            let observation =
                incomplete(decode(Utf16JsonProjection::parse(json).unwrap(), is_error));
            let parts = observation.parts();
            assert_eq!(parts.exit_code, expected);
            assert_eq!(
                parts.stdout,
                ReportedShellStream::Present {
                    utf16: "out".encode_utf16().collect(),
                    host_truncated: false
                }
            );
            assert_eq!(
                parts.stderr,
                ReportedShellStream::Present {
                    utf16: "err".encode_utf16().collect(),
                    host_truncated: false
                }
            );
            assert_eq!(parts.tool_result_is_error, is_error);
            assert_eq!(parts.combined_truncated, None);
            assert_eq!(parts.timed_out, None);
            assert_eq!(parts.payload_is_error, None);
            assert_eq!(parts.interrupted, None);
        }
    }
    for number in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let observation = incomplete(decode(number_payload(number), false));
        assert_eq!(
            observation.parts().exit_code,
            ReportedExitCode::NonFinite {
                bits: number.to_bits()
            }
        );
    }
}

#[test]
fn valid_exit_code_boundaries_are_preserved() {
    for code in [i32::MIN, -1, 0, 1, i32::MAX] {
        let decoded = match decode(number_payload(f64::from(code)), false) {
            Ok(decoded) => decoded,
            Err(_) => panic!("valid exit code was rejected"),
        };
        assert_eq!(decoded.result.exit_code, code);
        assert_eq!(decoded.result.stdout, "stdout");
        assert_eq!(decoded.result.stderr, "stderr");
    }
}

#[test]
fn incomplete_payload_preserves_exact_streams_and_reported_flags() {
    let payload = Utf16JsonProjection::parse(
        r#"{"stdout":"\ud800\ud83d\ude00","stderr":null,"truncated":true,"timed_out":false,"is_error":true,"interrupted":false}"#,
    ).unwrap();
    let observation = incomplete(decode(payload, false));
    let parts = observation.parts();
    assert_eq!(parts.exit_code, ReportedExitCode::Missing);
    assert_eq!(
        parts.stdout,
        ReportedShellStream::Present {
            utf16: vec![0xd800, 0xd83d, 0xde00],
            host_truncated: false
        }
    );
    assert_eq!(parts.stderr, ReportedShellStream::Null);
    assert_eq!(parts.combined_truncated, Some(true));
    assert_eq!(parts.timed_out, Some(false));
    assert!(!parts.tool_result_is_error);
    assert_eq!(parts.payload_is_error, Some(true));
    assert_eq!(parts.interrupted, Some(false));

    let payload = Utf16JsonProjection::parse(r#"{"stdout":false}"#).unwrap();
    let observation = incomplete(decode(payload, false));
    assert_eq!(observation.parts().stdout, ReportedShellStream::InvalidType);
    assert_eq!(observation.parts().stderr, ReportedShellStream::Missing);
}

#[test]
fn host_caps_exact_utf16_without_splitting_surrogate_pairs() {
    let mut units = vec![u16::from(b'a'); tool_api::MAX_UNVERIFIED_OUTPUT_CHARS - 1];
    units.extend_from_slice(&[0xd83d, 0xde00, u16::from(b'z')]);
    let mut builder = Utf16JsonBuilder::new();
    let stdout = builder.push_string_units(units);
    let root = builder
        .push_object(vec![("stdout".into(), stdout)])
        .unwrap();
    let observation = incomplete(decode(builder.finish(root).unwrap(), false));
    let ReportedShellStream::Present {
        utf16,
        host_truncated,
    } = &observation.parts().stdout
    else {
        panic!("stdout was not retained");
    };
    assert!(*host_truncated);
    assert_eq!(utf16.len(), tool_api::MAX_UNVERIFIED_OUTPUT_CHARS + 1);
    assert_eq!(&utf16[utf16.len() - 2..], &[0xd83d, 0xde00]);
}

#[test]
fn reported_payload_roundtrips_with_bounded_json_and_redacted_debug() {
    let mut units = Vec::with_capacity(tool_api::MAX_UNVERIFIED_OUTPUT_CHARS * 2 + 1);
    for _ in 0..tool_api::MAX_UNVERIFIED_OUTPUT_CHARS {
        units.extend_from_slice(&[0xd83d, 0xde00]);
    }
    units.push(u16::from(b'z'));
    let mut builder = Utf16JsonBuilder::new();
    let stdout = builder.push_string_units(units.clone());
    let stderr = builder.push_string_units(units);
    let root = builder
        .push_object(vec![("stdout".into(), stdout), ("stderr".into(), stderr)])
        .unwrap();
    let observation = incomplete(decode(builder.finish(root).unwrap(), false));
    let debug = format!("{observation:?}");
    assert!(!debug.contains("55357"));
    assert!(!debug.contains("56832"));
    let json = serde_json::to_vec(&*observation).unwrap();
    assert!(json.len() < 721_000, "one report must remain bounded");
    let recovered: ReportedShellPayload = serde_json::from_slice(&json).unwrap();
    assert_eq!(recovered.parts(), observation.parts());
}

#[tokio::test]
async fn malformed_result_keeps_the_bash_claim_user_gated() {
    use crate::mobile::test_support::FakeListener;
    use crate::mobile::turn_durability::QueueDeliveryState;
    use client::adapter::ListenerSink;
    use client::protocol::commands::PromptModeDto;
    use client::protocol::events::{ClientEvent, TurnRecoveryStateDto};

    let tmp = tempfile::tempdir().unwrap();
    let store = super::super::DurableTurnStore::new(tmp.path().join("turns"));
    store
        .admit_queued_prompt(
            "session",
            Some(257),
            Some(257),
            Utf16JsonProjection::from("printf partial"),
            Some(PromptModeDto::Bash),
            vec![],
        )
        .unwrap();
    store.claim_queued_bash("session", 257, true).unwrap();
    let turn = Arc::new(super::super::ActiveTurn::new_owned(
        Some(257),
        "session".into(),
        1,
    ));
    let listener = Arc::new(FakeListener::default());
    let sink = ListenerSink::arc(listener.clone());
    let payload = Utf16JsonProjection::parse(r#"{"stdout":"partial","exit_code":null}"#).unwrap();
    let Err(reason) = decode(payload, false) else {
        panic!("null exit status must not produce a completed Bash result");
    };
    super::super::report_cleanup_unverified(&turn, &store, &sink, &reason).await;
    let checkpoint = store.load("session", 257).unwrap();
    assert_eq!(checkpoint.state, Some(TurnRecoveryStateDto::WaitingForUser));
    assert_eq!(checkpoint.queue_delivery, QueueDeliveryState::BashClaimed);
    assert!(!checkpoint.safe_to_resume);
    assert!(checkpoint.bash_result.is_none());
    assert!(checkpoint
        .reason
        .as_deref()
        .unwrap()
        .starts_with("shell_result_incomplete:"));
    let events = listener.received.lock().await;
    assert!(events.iter().any(|event| matches!(event,
        ClientEvent::SystemNotice { message, is_error: true }
            if message.display().contains("process cleanup was not established"))));
    assert!(!events
        .iter()
        .any(|event| matches!(event, ClientEvent::TurnEnded { .. })));
}

#[tokio::test]
async fn direct_bash_preserves_malformed_tool_result_without_querying_model() {
    use orchestrator::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
    use tokio_util::sync::CancellationToken;

    let tmp = tempfile::tempdir().unwrap();
    let payload = Utf16JsonProjection::parse(
        r#"{"stdout":"partial","stderr":"warning","exit_code":2147483648,"truncated":true}"#,
    )
    .unwrap();
    let mut registry = tool_api::ToolRegistry::new();
    registry.register_builtin(Arc::new(MalformedShell(payload)));
    let api = Arc::new(MockApiClient::new(vec![]));
    let orchestrator = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        tmp.path().to_path_buf(),
    );
    let result = execute_mobile_bash_tool(
        &orchestrator,
        &Utf16JsonProjection::from("printf partial"),
        CancellationToken::new(),
    )
    .await;
    let observation = incomplete(result);
    assert_eq!(
        observation.parts().exit_code,
        ReportedExitCode::OutOfRange {
            bits: 2_147_483_648f64.to_bits()
        }
    );
    assert_eq!(
        observation.parts().stdout,
        ReportedShellStream::Present {
            utf16: "partial".encode_utf16().collect(),
            host_truncated: false
        }
    );
    assert_eq!(
        observation.parts().stderr,
        ReportedShellStream::Present {
            utf16: "warning".encode_utf16().collect(),
            host_truncated: false
        }
    );
    assert_eq!(observation.parts().combined_truncated, Some(true));
    assert_eq!(api.captured_requests().await.len(), 0);
}
