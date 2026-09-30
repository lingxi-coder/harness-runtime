//! Streaming transport failures propagate through conversation output.

use llm_runtime::{LlmError, LlmEvent};
use orchestrator::test_support::{
    message_start, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, OrchestratorError,
};
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn orch(
    streaming: Arc<MockStreamingApiClient>,
    output: Arc<MockOutputStream>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        streaming,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

/// #10 (faithful port of `query.ts:955-997` catch): a mid-stream model/runtime
/// error (here a `Transport` connection reset) is NOT a hard failure. The turn
/// ends GRACEFULLY with `reason:"model_error"` and the raw error text is surfaced
/// as an api-error assistant message — instead of the prior `OrchestratorError`
/// bubble (which showed a phantom "[Request interrupted by user]" to SDK callers).
///
/// The fixture is a TERMINAL error raised before any content, and both halves
/// of that are load-bearing:
///
/// - **Before any content**, because P1-04 (cc 2.1.199) owns the other half:
///   once a real content block has completed — or a transport close leaves
///   visible text whose stop frame was lost — the partial is finalized in place
///   and the byte-exact "API Error: Connection lost mid-response…" notice is
///   surfaced INSTEAD of the raw error. That case belongs to
///   `streaming_partial_finalize_test::transport_after_completed_block_finalizes_partial`.
///   This test kept a `Transport`-after-text fixture until 2026-08-20, so the two
///   asserted opposite outcomes for one scenario; P1-04 landed in `d7215d659`
///   without updating this one and it had been red ever since.
/// - **Terminal**, because a retryable transport error is retried rather than
///   surfaced, and the turn would end on whatever the retry produced. `TlsCert`
///   is never retried by construction (a handshake that cannot succeed only
///   burns the budget), so it reaches the graceful `model_error` catch directly
///   — which is the invariant #10 is about.
#[tokio::test]
async fn mid_stream_err_ends_turn_gracefully_as_model_error() {
    let turn: Vec<Result<LlmEvent, LlmError>> = vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Err(LlmError::TlsCert {
            code: "CERT_HAS_EXPIRED".into(),
            message: "certificate has expired (CERT_HAS_EXPIRED)".into(),
        }),
    ];
    let api = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![turn]));
    let output = Arc::new(MockOutputStream::new());
    let orch = orch(api, output.clone());

    let outcome = orch
        .run_turn_streaming("hi")
        .await
        .expect("a model/runtime error must end the turn gracefully, not bubble a hard error");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );

    let events = output.snapshot().await;
    // Terminal reason is `model_error`.
    assert!(
        events.iter().any(|e| matches!(
            e,
            platform_api::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error"
        )),
        "turn must end with stop_reason model_error; events={events:#?}"
    );
    // The raw error text is surfaced verbatim (createAssistantAPIErrorMessage,
    // NOT an `API Error:`-prefixed template).
    let texts = output.text_events().await;
    assert!(
        texts
            .iter()
            .any(|t| t.contains("certificate has expired (CERT_HAS_EXPIRED)")),
        "the raw error text must be surfaced as the model_error message; \
         texts={texts:#?}"
    );
}

/// #10 carve-out: a `RateLimited` error must STILL propagate as a hard
/// `OrchestratorError` so the `run_turn*` wrapper can re-map it onto the
/// limits-specific copy + emit the terminal rate-limit snapshot — it must NOT be
/// swallowed as a graceful `model_error`.
#[tokio::test]
async fn rate_limited_still_propagates_as_hard_error() {
    let api = Arc::new(MockStreamingApiClient::with_open_error(
        LlmError::RateLimited {
            retry_after: None,
            scope: None,
        },
        Vec::new(),
    ));
    let output = Arc::new(MockOutputStream::new());
    let orch = orch(api, output);

    let err = orch
        .run_turn_streaming("hi")
        .await
        .expect_err("RateLimited must remain a hard error for wrapper enrichment");
    assert!(
        matches!(
            err,
            OrchestratorError::Streaming(LlmError::RateLimited { .. })
                | OrchestratorError::ApiCall(LlmError::RateLimited { .. })
        ),
        "expected a RateLimited hard error, got {err:?}"
    );
}
