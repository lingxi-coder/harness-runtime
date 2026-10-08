//! [`EffectHandler`] contract.
//!
//! Verifies that an `EffectHandler` impl accepts every reducer-emitted
//! [`Effect`] variant without panicking, and that the return type is one
//! of `Ok(EffectResult)` or a documented `EffectError`. Real wiring (the
//! cli-demo's run loop, the SDK consumer's stream handler, etc.) layers
//! richer behavioural tests on top of this surface.
//!
//! Invariants:
//!
//! * Each [`Effect`] variant is accepted without panic / abort.
//! * The handler returns `Result<EffectResult, EffectError>` — i.e. there
//!   is no `unreachable!` / `unimplemented!` lurking in the dispatch.
//! * Render variants (`RenderStreamDelta`, `RenderError`,
//!   `RenderTokenUsageUpdate`) return `Ok(EffectResult::Ack)` for ack-shaped
//!   handlers; the contract does not enforce this strictly because real
//!   handlers may surface a fail-closed `Internal` error if their UI sink
//!   has died.
//!
//! The contract dispatches a representative slice of variants — one per
//! subsystem — rather than the full enum, so it stays useful as the
//! [`Effect`] enum grows without needing a churn-per-variant update.

use lingxi_core::host::EffectHandler;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, Effect, RedactableContent, RequestId, SessionId, ToolUseId,
};

/// Run the standard [`EffectHandler`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn effect_handler_contract_tests<H: EffectHandler>(h: &H) {
    test_render_stream_delta_handled(h).await;
    test_render_error_handled(h).await;
    test_render_token_usage_update_handled(h).await;
    test_send_api_request_handled(h).await;
    test_persist_session_snapshot_handled(h).await;
    test_load_session_handled(h).await;
    test_terminate_handled(h).await;
    test_evaluate_permission_handled(h).await;
    test_scan_for_secrets_handled(h).await;
}

async fn test_render_stream_delta_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::RenderStreamDelta {
            text: "contract-delta".to_string(),
        })
        .await;
}

async fn test_render_error_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::RenderError {
            error: "contract-error".to_string(),
        })
        .await;
}

async fn test_render_token_usage_update_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::RenderTokenUsageUpdate {
            input_tokens: 1,
            output_tokens: 2,
        })
        .await;
}

async fn test_send_api_request_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::SendApiRequest {
            request_id: RequestId::nil(),
            request_body: Utf16JsonProjection::plain(
                serde_json::json!({ "model": "contract-test" }),
            ),
        })
        .await;
}

async fn test_persist_session_snapshot_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::PersistSessionSnapshot {
            session_id: SessionId::nil(),
            snapshot: serde_json::json!({}),
        })
        .await;
}

async fn test_load_session_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::LoadSession {
            session_id: SessionId::nil(),
        })
        .await;
}

async fn test_terminate_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::Terminate {
            reason: "contract-test".to_string(),
        })
        .await;
}

async fn test_evaluate_permission_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::EvaluatePermission {
            tool_use_id: ToolUseId::from("toolu_contract"),
            tool_name: "Read".to_string(),
            input: serde_json::json!({ "path": "/tmp/contract" }),
        })
        .await;
}

async fn test_scan_for_secrets_handled<H: EffectHandler>(h: &H) {
    let _ = h
        .handle(Effect::ScanForSecrets {
            boundary: "contract_test".to_string(),
            content: RedactableContent::new("contract-scan-input".to_string()),
        })
        .await;
}

// ---------------------------------------------------------------------------
// In-harness ack-everything handler. Used by the driver tests so the
// contract has a concrete subject without depending on a downstream crate
// owning an `EffectHandler` impl.
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use lingxi_core::types::{EffectError, EffectResult};

/// Minimal [`EffectHandler`] that acks every variant. Suitable as a
/// stand-in subject for the contract suite; real handlers are exercised
/// by their owning crates (e.g. `lingxi-core` once a router lands).
#[derive(Debug, Default)]
pub struct AckEffectHandler;

#[async_trait]
impl EffectHandler for AckEffectHandler {
    async fn handle(&self, _effect: Effect) -> Result<EffectResult, EffectError> {
        Ok(EffectResult::Ack)
    }
}
