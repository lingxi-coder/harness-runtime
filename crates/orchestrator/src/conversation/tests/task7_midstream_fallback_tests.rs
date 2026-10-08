use super::super::drivers_impl::is_env_truthy;
use super::*;
use crate::test_support::{
    content_block_start_text, message_start, mock_message_response, noop_hook_executor, text_delta,
    MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use llm_runtime::ContentBlock as LlmContentBlock;
use platform_posix::mcp::PosixMcpTransport;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

const DISABLE_FALLBACK_ENV: &str = "LINGXI_DISABLE_NONSTREAMING_FALLBACK";

/// Serializes the midstream tests that read/write `DISABLE_FALLBACK_ENV`.
///
/// `std::env::set_var` / `remove_var` are not thread-safe when other threads
/// read the same variable concurrently.  Tokio runs `#[tokio::test]` functions
/// in the same process and may schedule them in parallel; holding this lock for
/// the duration of each test makes the pair race-free without any new crate dep.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ClosedFrameFallbackApi {
    api: Arc<MockApiClient>,
    output: Arc<MockOutputStream>,
}

#[async_trait::async_trait]
impl OrchestratorApiClient for ClosedFrameFallbackApi {
    async fn messages_create(
        &self,
        request: crate::OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        let frames = self.output.partial_stream_event_snapshot().await;
        assert_eq!(
            &frames[frames.len() - 2..],
            &[
                "{\"type\":\"content_block_stop\",\"index\":0}".to_string(),
                "{\"type\":\"message_stop\"}".to_string(),
            ],
            "forwarded frames close before the non-streaming request starts"
        );
        self.api.messages_create(request).await
    }
}

/// Build a one-ContentBlockStart-then-Err(Overloaded) stream: the first
/// event is yielded successfully (proving partial events arrived), then the
/// stream errors with `LlmError::Overloaded`.
fn one_event_then_overloaded() -> Vec<Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>> {
    vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "partial")),
        Err(llm_runtime::LlmError::Overloaded { repeated: false }),
    ]
}

/// Build an `end_turn` non-streaming response for the fallback.
fn fallback_response() -> llm_runtime::HistoryResponse {
    mock_message_response(
        vec![LlmContentBlock::Text {
            text: "fallback body".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )
}

/// Task 7 Step 1 (a)(b)(c)(d):
/// A stream that yields one ContentBlockStart then Err(Overloaded):
/// (a) the stream is NOT replayed (streaming_api called exactly once),
/// (b) a fresh non-streaming `seeded main request` is issued,
/// (c) the seed is 1 (streaming 529 counts toward the budget),
/// (d) the final response is built from the non-streaming reply only.
///
/// Parity: claude.ts:2551-2594, withRetry.ts:186
/// (`initialConsecutive529Errors: is529Error(streamingError) ? 1 : 0`)
#[tokio::test]
async fn midstream_529_triggers_nonstreaming_fallback() {
    // Serialize with the sibling test that also reads/writes DISABLE_FALLBACK_ENV.
    // `set_var`/`remove_var` are not thread-safe; the (tokio) mutex makes the
    // pair race-free without a new crate dependency, and its guard is safe to
    // hold across the .await points below.
    let _guard = ENV_LOCK.lock().await;
    // Ensure fallback is ENABLED for this test.
    std::env::remove_var(DISABLE_FALLBACK_ENV);

    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        one_event_then_overloaded(),
    ]));
    let api = Arc::new(MockApiClient::new(vec![fallback_response()]));
    let output = Arc::new(MockOutputStream::new().with_partial_stream_events());
    let transport = Arc::new(PosixMcpTransport::new());
    let registry = Arc::new(mcp::McpRegistry::with_raw_conn(
        transport.clone(),
        transport,
    ));
    let script = r#"import sys,json
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r: continue
 method=r.get('method')
 if method=='server/discover':
  v={'supportedVersions':['2026-07-28'],'capabilities':{},'instructions':'Read the project index first.'}
 elif method=='initialize':
  v={'protocolVersion':'2025-11-25','capabilities':{},'serverInfo':{'name':'records','version':'286'},'instructions':'Read the project index first.'}
 else: v={}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)
"#;
    let server = mcp::build_server_from_json_entry(
        "records",
        &serde_json::json!({"command":"python3", "args":["-u","-c",script]}),
        mcp::ConfigScope::Dynamic,
    )
    .unwrap();
    registry.connect(server).await.unwrap();
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                output_style: Some("Explanatory".into()),
                fallback_model: Some("fallback-first, fallback-second".into()),
                ..OrchestratorConfig::default()
            },
            Arc::new(ClosedFrameFallbackApi {
                api: api.clone(),
                output: output.clone(),
            }),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_mcp_registry(registry.clone()),
    );

    let outcome = orch
        .run_turn_streaming("hello")
        .await
        .expect("turn must succeed via fallback");

    let frames = output.partial_stream_event_snapshot().await;
    let frames: Vec<serde_json::Value> = frames
        .iter()
        .map(|frame| serde_json::from_str(frame).unwrap())
        .collect();
    let starts: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter_map(|(index, frame)| (frame["type"] == "message_start").then_some(index))
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "the non-streaming response has no raw SSE start frame"
    );
    assert_eq!(
        frames[frames.len() - 1],
        serde_json::json!({"type":"message_stop"})
    );
    assert_eq!(
        frames[frames.len() - 2],
        serde_json::json!({"type":"content_block_stop","index":0})
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "message_stop")
            .count(),
        1
    );

    // (a) Stream was called exactly once — NOT replayed.
    let stream_calls = streaming.captured_calls().await;
    assert_eq!(
        stream_calls.len(),
        1,
        "(a) stream must be called exactly once"
    );

    // (b) A fresh non-streaming seeded main request was called.
    let seeds = api.captured_seeds().await;
    assert_eq!(
        seeds.len(),
        1,
        "(b) seeded main request must be called exactly once"
    );

    // (c) The seed is 1 (the streaming 529 counts toward the consecutive 529 budget).
    assert_eq!(
        seeds[0], 1,
        "(c) seed must be 1 for a streaming Overloaded error"
    );

    let requests = api.captured_requests().await;
    let [crate::OrchestratorApiRequest::Main(request)] = requests.as_slice() else {
        panic!("expected the single owned main fallback request");
    };
    assert_eq!(request.opts.initial_consecutive_overloaded, Some(1));
    assert_eq!(
        request.opts.fallback,
        llm_runtime::FallbackPolicy::Models(vec![
            "fallback-first".into(),
            "fallback-second".into()
        ])
    );

    // The rebuilt fallback must keep just-created durable attachments in the
    // same positions among transient reminders as the first stream request.
    // Compare bytes rather than additional-context IDs, which are regenerated.
    let fallback_calls = api.captured_msgs().await;
    let texts = |messages: &[ConversationMessage]| {
        messages
            .iter()
            .map(ConversationMessage::text_content)
            .collect::<Vec<_>>()
    };
    let original = texts(&stream_calls[0].messages);
    assert_eq!(
        texts(&fallback_calls[0]),
        original,
        "fallback must preserve reminder order and bytes"
    );
    let style = original
        .iter()
        .position(|text| text.contains("Explanatory output style is active"))
        .unwrap();
    let instructions = original
        .iter()
        .position(|text| text.contains("# MCP Server Instructions"))
        .unwrap();
    let tokens = original
        .iter()
        .position(|text| text.contains("<total_tokens>"))
        .unwrap();
    assert!(instructions < style && style < tokens);
    for reminder in [
        &stream_calls[0].messages[instructions],
        &stream_calls[0].messages[tokens],
    ] {
        assert_eq!(
            fallback_calls[0]
                .iter()
                .filter(|message| message.id() == reminder.id())
                .count(),
            1,
            "fallback must reuse each durable reminder exactly once"
        );
    }
    registry.disconnect("records").await.unwrap();

    // (d) The final turn outcome is built from the non-streaming reply only.
    // The output must contain "fallback body" (from the non-streaming response),
    // NOT just "partial" (the partial stream events are discarded).
    let events = output.snapshot().await;
    let texts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            lingxi_core::host::OutputEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        texts.contains(&"fallback body"),
        "(d) output must contain the fallback body; texts={texts:?}"
    );
    // The turn must have ended with end_turn (not an error).
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "outcome must be EndTurn after non-streaming fallback; got {outcome:?}"
    );

    // M1 (Task 7 review): the PERSISTED assistant message must contain ONLY the
    // fallback body, not the partial streaming fragments.  TS yields deltas live
    // (claude.ts:2210 `yield m` fires inside the for-await loop at each
    // `content_block_stop`), so partial output reaching callers before the fallback
    // is parity — but the final persisted turn must reflect ONLY the fallback result.
    let session_arc = orch.session();
    let session_guard = session_arc.lock().await;
    let final_assistant = session_guard
        .history
        .iter()
        .filter_map(|msg| match msg {
            ConversationMessage::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .next_back()
        .expect("session must contain at least one assistant message");
    let persisted_texts: Vec<&str> = final_assistant
        .iter()
        .filter_map(|blk| match blk {
            lingxi_core::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        persisted_texts,
        vec!["fallback body"],
        "M1: final persisted assistant message must contain ONLY the fallback body; \
         got {persisted_texts:?}"
    );
}

#[tokio::test]
async fn midstream_non_overload_preserves_explicit_zero_seed_in_owned_request() {
    let _guard = ENV_LOCK.lock().await;
    std::env::remove_var(DISABLE_FALLBACK_ENV);
    let mut events = one_event_then_overloaded();
    *events.last_mut().unwrap() = Err(llm_runtime::LlmError::ProviderInternal);
    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![events]));
    let api = Arc::new(MockApiClient::new(vec![fallback_response()]));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            fallback_model: Some("fallback-first, fallback-second".into()),
            ..Default::default()
        },
        api.clone(),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    orch.run_turn_streaming("hello")
        .await
        .expect("fresh fallback must complete");
    assert_eq!(streaming.captured_calls().await.len(), 1);
    let requests = api.captured_requests().await;
    let [crate::OrchestratorApiRequest::Main(request)] = requests.as_slice() else {
        panic!("expected one actual fallback request")
    };
    assert_eq!(request.opts.initial_consecutive_overloaded, Some(0));
    assert_eq!(
        request.opts.fallback,
        llm_runtime::FallbackPolicy::Models(vec![
            "fallback-first".into(),
            "fallback-second".into()
        ])
    );
    assert_eq!(api.captured_seeds().await, [0]);
}

/// Task 7 Step 1 (twin with LINGXI_DISABLE_NONSTREAMING_FALLBACK=1):
/// When the env gate is set, the streaming error propagates instead of
/// triggering the non-streaming fallback.
///
/// Parity: claude.ts:2476-2501 (disableFallback branch).
#[tokio::test]
async fn midstream_529_propagates_when_fallback_disabled() {
    // Serialize with the sibling test that also reads/writes DISABLE_FALLBACK_ENV.
    // `set_var`/`remove_var` are not thread-safe; the (tokio) mutex makes the
    // pair race-free without a new crate dependency, and its guard is safe to
    // hold across the .await points below.
    let _guard = ENV_LOCK.lock().await;
    // Set the disable flag for this test.
    std::env::set_var(DISABLE_FALLBACK_ENV, "1");

    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        one_event_then_overloaded(),
    ]));
    let api = Arc::new(MockApiClient::new(vec![]));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        api.clone(),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    let result = orch.run_turn_streaming("hello").await;

    // Restore env BEFORE assertions to avoid leaking even on panic.
    std::env::remove_var(DISABLE_FALLBACK_ENV);

    // The error MUST propagate — no fallback.
    assert!(
        result.is_err(),
        "error must propagate when fallback is disabled"
    );
    // No non-streaming call was made.
    assert!(
        api.captured_seeds().await.is_empty(),
        "seeded main request must NOT be called when fallback is disabled"
    );
    // The stream was called exactly once.
    assert_eq!(
        streaming.captured_calls().await.len(),
        1,
        "stream was called exactly once"
    );
}

/// `is_env_truthy` covers the exact semantics of TS `isEnvTruthy`
/// (`utils/envUtils.ts:32`): truthy ONLY for the whitelist
/// `1`/`true`/`yes`/`on`, case-insensitive and trimmed; everything
/// else (including `no`/`off`/`2`/`enabled`/arbitrary strings) is
/// falsy.
#[test]
fn is_env_truthy_matches_ts_semantics() {
    // Not set → not truthy.
    assert!(!is_env_truthy(None));
    // Empty → not truthy.
    assert!(!is_env_truthy(Some("")));
    // "false" → not truthy.
    assert!(!is_env_truthy(Some("false")));
    // "0" → not truthy.
    assert!(!is_env_truthy(Some("0")));
    // Whitelist members → truthy.
    assert!(is_env_truthy(Some("1")));
    assert!(is_env_truthy(Some("true")));
    assert!(is_env_truthy(Some("yes")));
    assert!(is_env_truthy(Some("on")));
    // Case-insensitive + trimmed.
    assert!(is_env_truthy(Some("ON")));
    assert!(is_env_truthy(Some(" TRUE ")));
    assert!(is_env_truthy(Some("Yes")));
    // Non-whitelist values → NOT truthy (strict whitelist).
    assert!(!is_env_truthy(Some("no")));
    assert!(!is_env_truthy(Some("off")));
    assert!(!is_env_truthy(Some("2")));
    assert!(!is_env_truthy(Some("enabled")));
    assert!(!is_env_truthy(Some("disable")));
    assert!(!is_env_truthy(Some("random")));
}
