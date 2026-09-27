//! In-Loop Compaction Batch 4 — proactive pre-call compaction trigger.
//!
//! Verifies that the batched turn loop runs the proactive
//! snip+micro+autocompact pass BEFORE each model call (TS pre-call pipeline
//! `query.ts:365-467`):
//!
//! - With a compactor wired at a low test threshold and a history already over
//!   that threshold, running a turn compacts history (boundary marker present),
//!   the FIRST `messages_create` carries the compacted (not the seeded) history,
//!   and a `CompactionCompleted` event is emitted.
//! - With the history under the threshold (or no compactor wired), the trigger
//!   is a strict NO-OP: history is identical and no `CompactionCompleted` fires.
use llm_runtime::ContentBlock as LlmContentBlock;

use compaction::CompactionOrchestrator;
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::OutputEvent;
use protocol::{ConversationMessage, MessageId};
use std::sync::Arc;

/// Build an orchestrator with a `MockApiClient` (single `end_turn` response)
/// and an optional compactor at `threshold`. Returns the orch + the api mock so
/// the test can inspect `captured_msgs()`.
fn make_orch(
    threshold: Option<u64>,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<MockApiClient>,
    Arc<MockOutputStream>,
) {
    make_orch_with(threshold, false)
}

/// [`make_orch`], optionally wiring the compactor with
/// `with_model_derived_threshold` so the gate follows the session's model
/// instead of the fixed constructor value.
fn make_orch_with(
    threshold: Option<u64>,
    model_derived: bool,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<MockApiClient>,
    Arc<MockOutputStream>,
) {
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".to_string(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        memory,
        std::env::temp_dir(),
    );
    if let Some(t) = threshold {
        let compactor = CompactionOrchestrator::new(t);
        let compactor = if model_derived {
            compactor.with_model_derived_threshold()
        } else {
            compactor
        };
        orch = orch.with_compaction(Arc::new(compactor));
    }
    (Arc::new(orch), api, output)
}

/// Register `id` with the process-global model registry so
/// `context_window_for_model` answers with the real catalog window instead of
/// the 200k Claude default.
fn register_model(id: &str, context_window: u64, max_output_tokens: u64) {
    llm_runtime::model::model_limits::register(
        id,
        llm_runtime::model::model_limits::ModelLimits {
            context_window,
            max_output_tokens,
        },
    );
}

async fn set_session_model(orch: &ConversationOrchestrator, model: &str) {
    orch.session().lock().await.model = model.to_string();
}

/// Seed `n` filler user messages so the token estimate clears a small threshold.
async fn seed_history(orch: &ConversationOrchestrator, n: usize) {
    let session = orch.session();
    let mut s = session.lock().await;
    for i in 0..n {
        s.history.push(ConversationMessage::user(
            MessageId::new(),
            format!("turn-{i} body padded with filler text to push token count up beyond autocompact threshold"),
        ));
    }
}

async fn history(orch: &ConversationOrchestrator) -> Vec<ConversationMessage> {
    let session = orch.session();
    let s = session.lock().await;
    s.history.clone()
}

#[tokio::test]
async fn proactive_compacts_over_threshold_and_next_call_carries_compacted_history() {
    // Very low threshold (100 tokens) + a big seeded history => even after
    // snip's 10-message protected-tail floor the estimate stays over threshold,
    // so autocompact (not snip alone) fires in the proactive pre-call trigger
    // before the first model call.
    let (orch, api, output) = make_orch(Some(100));
    seed_history(&orch, 60).await;
    let before = history(&orch).await.len();
    assert!(before >= 60, "seed should produce a large history");

    orch.run_turn("hello").await.expect("turn ok");

    // History after the turn = the compacted summary set + the `[Compacted N
    // → M]` boundary marker + the user prompt + the assistant reply. The key
    // proactive assertion: a System boundary marker is present and the total is
    // far smaller than the seeded 60+.
    let after = history(&orch).await;
    let boundary = after.iter().find_map(|m| match m {
        // CSM.4: the boundary is the TS "Conversation compacted" sentinel.
        ConversationMessage::System { content, .. } if content == "Conversation compacted" => {
            Some(content.clone())
        }
        _ => None,
    });
    assert!(
        boundary.is_some(),
        "post-compact boundary marker must be present; history = {after:#?}"
    );
    assert!(
        after.len() < before,
        "compacted history ({}) must be smaller than the seeded history ({before})",
        after.len()
    );

    // The FIRST (and only) messages_create must have carried the COMPACTED
    // history, not the seeded 60+ messages. The proactive trigger ran before
    // the snapshot, so the captured msgs are short.
    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1, "exactly one batched API call");
    assert!(
        calls[0].len() < before,
        "the model call must carry the compacted history ({} msgs), not the seeded {before}",
        calls[0].len()
    );
    // The compacted call must contain the boundary marker (it was applied to
    // session.history before the snapshot).
    let call_has_boundary = calls[0].iter().any(|m| {
        matches!(m, ConversationMessage::System { content, .. } if content == "Conversation compacted")
    });
    assert!(
        call_has_boundary,
        "the model call's messages must include the compaction boundary marker"
    );

    assert_eq!(
        output.compaction_phase_snapshot().await,
        ["preparing", "summarizing", "restoring", "complete"]
    );

    // CompactionCompleted must have been emitted.
    let events = output.snapshot().await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::CompactionCompleted { .. })),
        "CompactionCompleted event must be emitted; events = {events:#?}"
    );
}

#[tokio::test]
async fn under_threshold_is_strict_noop() {
    // High threshold (10M tokens) => the small seeded history is well under it,
    // so the proactive trigger must be a strict no-op: history unchanged (apart
    // from the turn's own user+assistant append) and NO CompactionCompleted.
    let (orch, _api, output) = make_orch(Some(10_000_000));
    seed_history(&orch, 3).await;
    let before = history(&orch).await;

    orch.run_turn("hello").await.expect("turn ok");

    assert!(output.compaction_phase_snapshot().await.is_empty());
    let after = history(&orch).await;
    // No boundary marker anywhere.
    assert!(
        !after.iter().any(|m| matches!(
            m,
            ConversationMessage::System { content, .. } if content == "Conversation compacted"
        )),
        "no compaction boundary marker may appear under threshold"
    );
    // The seeded prefix is preserved verbatim (proactive trigger did not touch
    // it); the turn only appended the user prompt + assistant reply on top.
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "the seeded history prefix must be untouched under threshold"
    );

    let events = output.snapshot().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutputEvent::CompactionCompleted { .. })),
        "no CompactionCompleted may fire under threshold"
    );
}

#[tokio::test]
async fn gate_follows_a_wide_catalog_model_instead_of_the_engine_threshold() {
    // The engine used to wire the compactor with a fixed 150_000-token ceiling
    // — a constant sized for the 200k Anthropic window — so a 1M-token catalog
    // model was compacted at ~15% of its context. With
    // `with_model_derived_threshold` the gate resolves the session model's real
    // window, so the same small history stays untouched even though the
    // compactor was constructed with a 100-token threshold.
    register_model("test-gate-wide-model", 1_000_000, 384_000);
    let (orch, _api, output) = make_orch_with(Some(100), true);
    set_session_model(&orch, "test-gate-wide-model").await;
    seed_history(&orch, 60).await;
    let before = history(&orch).await;

    orch.run_turn("hello").await.expect("turn ok");

    assert!(
        output.compaction_phase_snapshot().await.is_empty(),
        "a 1M-window model must not compact a ~1.5k-token history"
    );
    let after = history(&orch).await;
    assert!(
        !after.iter().any(|m| matches!(
            m,
            ConversationMessage::System { content, .. } if content == "Conversation compacted"
        )),
        "no compaction boundary may appear below the model's own threshold"
    );
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "the seeded history prefix must be untouched"
    );
}

#[tokio::test]
async fn gate_compacts_a_narrow_model_despite_a_huge_engine_threshold() {
    // The inverse direction: a model whose real window is small must compact
    // even when the compactor was constructed with a 10M-token threshold.
    // 30k window − min(1k, 20k) reserved output − 13k buffer ⇒ a 16k threshold,
    // which 1000 filler messages (~21k estimated tokens) clear.
    register_model("test-gate-narrow-model", 30_000, 1_000);
    // `USER_TYPE=ant` + `LINGXI_MAX_CONTEXT_TOKENS` would override every
    // model's window process-wide; neutralize it for the assertion and restore.
    let saved_max_context = std::env::var("LINGXI_MAX_CONTEXT_TOKENS").ok();
    std::env::remove_var("LINGXI_MAX_CONTEXT_TOKENS");

    let (orch, _api, output) = make_orch_with(Some(10_000_000), true);
    set_session_model(&orch, "test-gate-narrow-model").await;
    seed_history(&orch, 1000).await;
    let before = history(&orch).await.len();

    let turn = orch.run_turn("hello").await;
    match saved_max_context {
        Some(value) => std::env::set_var("LINGXI_MAX_CONTEXT_TOKENS", value),
        None => std::env::remove_var("LINGXI_MAX_CONTEXT_TOKENS"),
    }
    turn.expect("turn ok");

    let events = output.snapshot().await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::CompactionCompleted { .. })),
        "the model's own window, not the engine threshold, must gate autocompact"
    );
    assert!(
        history(&orch).await.len() < before,
        "the compacted history must be smaller than the seeded {before} messages"
    );
}

#[tokio::test]
async fn no_compactor_wired_is_strict_noop() {
    // No compactor at all => proactive trigger is a strict no-op even with a
    // large history.
    let (orch, _api, output) = make_orch(None);
    seed_history(&orch, 60).await;
    let before = history(&orch).await;

    orch.run_turn("hello").await.expect("turn ok");

    assert!(output.compaction_phase_snapshot().await.is_empty());
    let after = history(&orch).await;
    assert!(
        !after.iter().any(|m| matches!(
            m,
            ConversationMessage::System { content, .. } if content == "Conversation compacted"
        )),
        "no boundary marker when no compactor is wired"
    );
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "seeded history prefix untouched when no compactor is wired"
    );
    let events = output.snapshot().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, OutputEvent::CompactionCompleted { .. })),
        "no CompactionCompleted when no compactor is wired"
    );
}
