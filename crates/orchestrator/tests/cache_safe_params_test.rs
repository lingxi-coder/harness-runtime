//! In-Loop Compaction Batch 6 — cache-safe prompt-prefix snapshotting.
//!
//! Verifies the PRODUCER side of the forked-summarizer cache-sharing contract:
//! before compaction and the API call, and again after success, the turn loop writes a
//! `sidequery::CacheSafeParams` into the shared slot whose `fork_context_messages`
//! is exactly the message set the model saw, so the forked autocompact
//! summarizer (wired at the composition root via
//! `Autocompactor::with_forked_runner` with the SAME slot) can replay the prefix
//! verbatim and hit Anthropic's prompt cache. Without a slot wired the save is a
//! strict no-op.
use llm_runtime::ContentBlock as LlmContentBlock;

use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use protocol::{ConversationMessage, MessageId};
use sidequery::CacheSafeParamsSlot;
use std::sync::Arc;

/// Build an orchestrator with a single `end_turn` mock response and an OPTIONAL
/// cache-safe slot. Returns orch + api mock + the slot handle (so the test can
/// inspect `get_last()`).
fn make_orch(
    slot: Option<Arc<CacheSafeParamsSlot>>,
) -> (Arc<ConversationOrchestrator>, Arc<MockApiClient>) {
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".to_string(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    if let Some(slot) = slot {
        orch = orch.with_cache_safe_slot(slot);
    }
    (Arc::new(orch), api)
}

async fn seed_history(orch: &ConversationOrchestrator, n: usize) {
    let session = orch.session();
    let mut s = session.lock().await;
    for i in 0..n {
        s.history.push(ConversationMessage::user(
            MessageId::new(),
            format!("seed message {i}"),
        ));
    }
}

#[tokio::test]
async fn turn_populates_slot_with_exact_sent_prefix() {
    let slot = Arc::new(CacheSafeParamsSlot::new());
    let (orch, api) = make_orch(Some(slot.clone()));
    seed_history(&orch, 3).await;

    // Nothing saved before the first turn.
    assert!(slot.get_last().await.is_none(), "slot starts empty");

    orch.run_turn("hello").await.expect("turn ok");

    let saved = slot
        .get_last()
        .await
        .expect("a cache-safe snapshot must be saved after a successful call");

    // Proactive and reactive compaction each receive a pre-call snapshot;
    // the successful response refreshes the same prefix before reply insertion.
    assert_eq!(
        saved.generation, 3,
        "two pre-call saves plus the success save"
    );

    // `fork_context_messages` is the cache-safe fork prefix — claude-code's
    // `cacheSafeParams.forkContextMessages = re` (`session.history`), captured
    // BEFORE the leading `additionalContext` meta message is prepended by `A6n`
    // at `callModel` time. The sent messages carry that leading context plus
    // the transient total-tokens reminder after the saved fork prefix.
    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1, "exactly one batched API call");
    assert_eq!(
        saved.fork_context_messages.len() + 2,
        calls[0].len(),
        "sent messages = additionalContext + saved fork prefix + total_tokens"
    );
    assert_eq!(
        saved.fork_context_messages,
        calls[0][1..calls[0].len() - 1],
        "saved fork_context_messages must exclude transient reminders"
    );

    // The model id propagates into the snapshot's tool-use options.
    assert_eq!(
        saved.tool_use_options.main_loop_model,
        OrchestratorConfig::default().model,
    );
}

#[tokio::test]
async fn failed_first_call_keeps_the_pre_call_snapshot_without_a_success_save() {
    let slot = Arc::new(CacheSafeParamsSlot::new());
    let (orch, api) = make_orch(Some(slot.clone()));
    seed_history(&orch, 3).await;
    api.set_fail_with(Some(llm_runtime::LlmError::RateLimited {
        retry_after: None,
        scope: None,
    }));

    orch.run_turn("hello")
        .await
        .expect_err("terminal rate limit");

    let saved = slot
        .get_last()
        .await
        .expect("the first call must seed the compact fork even if no API call has succeeded");
    assert_eq!(
        saved.generation, 2,
        "failed calls do not perform a success save"
    );
    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        saved.fork_context_messages,
        calls[0][1..calls[0].len() - 1],
        "pre-call snapshot excludes the same transient reminders as the success snapshot"
    );
    assert_eq!(
        saved.tools,
        api.captured_tools().await[0],
        "a first-call recovery fork has the parent tool definitions available"
    );
}

#[tokio::test]
async fn no_slot_wired_is_a_strict_noop() {
    // Without a wired slot the turn runs normally and nothing is saved anywhere
    // (the save helper early-returns before cloning history). We can't inspect a
    // non-existent slot, so the guarantee is simply that the turn succeeds.
    let (orch, api) = make_orch(None);
    seed_history(&orch, 2).await;
    orch.run_turn("hello")
        .await
        .expect("turn ok without a slot");
    assert_eq!(api.captured_msgs().await.len(), 1, "one API call, no panic");
}
