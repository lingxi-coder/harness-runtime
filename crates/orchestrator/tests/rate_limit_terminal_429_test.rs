//! Task 6 (llm-runtime future-work batch 5): limits-specific terminal 429 copy.
//!
//! When a turn DIES on a 429 (retries exhausted), claude-code composes the
//! rejected-branch limits copy from the error's own unified headers and makes
//! it the user-visible error content (`errors.ts:480-516` →
//! `getRateLimitErrorMessage` → `rateLimitMessages.ts:333-344`):
//!
//! ```text
//! You've hit your weekly limit · resets <time>
//! ```
//!
//! The Rust seam: `ProviderApiAdapter` records the 429 error response's
//! unified headers and caches the composed copy; the orchestrator's public
//! turn drivers re-map a terminal `ApiCall(RateLimited)` /
//! `Streaming(RateLimited)` into `OrchestratorError::RateLimitRejected`,
//! whose `Display` IS the composed copy. With no unified-header context the
//! generic `"api call failed: rate limited"` surface is unchanged.

use llm_runtime::LlmError;
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig, OrchestratorError};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn build_orch(api: Arc<MockApiClient>, output: Arc<MockOutputStream>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

/// Terminal 429 + a composed limits copy cached by the API client → the
/// turn's `Err` Display is EXACTLY the composed copy (byte-pinned against
/// `rateLimitMessages.ts:333-344` `formatLimitReachedText('weekly limit', …)`).
#[tokio::test]
async fn terminal_429_with_limits_context_surfaces_composed_copy() {
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_fail_with(Some(LlmError::RateLimited {
        retry_after: None,
        scope: None,
    }));
    api.set_rate_limit_error_message(Some(
        "You've hit your weekly limit · resets 3pm".to_string(),
    ));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api, output);

    let err = orch
        .run_turn("hello")
        .await
        .expect_err("turn must die on 429");
    assert!(
        matches!(err, OrchestratorError::RateLimitRejected { .. }),
        "terminal 429 with limits context must map to RateLimitRejected, got {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "You've hit your weekly limit · resets 3pm",
        "Display must be the composed getRateLimitErrorMessage copy verbatim"
    );
}

/// Terminal 429 WITHOUT a composed copy (the 429 carried no unified headers —
/// the `if (rateLimitType || overageStatus)` gate at errors.ts:480 fails) →
/// the existing generic surface is unchanged.
#[tokio::test]
async fn terminal_429_without_limits_context_keeps_generic_copy() {
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_fail_with(Some(LlmError::RateLimited {
        retry_after: None,
        scope: None,
    }));
    // No composed message cached (default None).
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api, output);

    let err = orch
        .run_turn("hello")
        .await
        .expect_err("turn must die on 429");
    assert!(
        matches!(
            err,
            OrchestratorError::ApiCall(LlmError::RateLimited { .. })
        ),
        "no limits context → unchanged ApiCall(RateLimited), got {err:?}"
    );
    assert_eq!(err.to_string(), "api call failed: rate limited");
}

/// A non-429 terminal error that still BUBBLES must NOT consult the
/// composed-copy cache: even with a stale rate-limit message cached, it keeps its
/// own surface. Uses `Overloaded` — a non-RateLimited carve-out that still
/// propagates post-#10 (generic errors like Transport now end gracefully as
/// `model_error` and never reach `enrich_api_error`, so they can't exercise the
/// cache-selectivity this test guards).
#[tokio::test]
async fn non_429_terminal_error_ignores_cached_limits_copy() {
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_fail_with(Some(LlmError::Overloaded { repeated: false }));
    api.set_rate_limit_error_message(Some(
        "You've hit your weekly limit · resets 3pm".to_string(),
    ));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api, output);

    let err = orch.run_turn("hello").await.expect_err("turn must die");
    assert!(
        matches!(err, OrchestratorError::ApiCall(LlmError::Overloaded { .. })),
        "non-429 error must pass through untouched, got {err:?}"
    );
    assert_eq!(err.to_string(), "api call failed: provider overloaded");
}
