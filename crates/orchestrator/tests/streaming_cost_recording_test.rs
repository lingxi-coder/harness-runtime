//! Streaming-turn billing gap: verify `CostTracker` receives usage after
//! `run_turn_streaming` (was never recorded before this fix).
//!
//! Mirror of `cost_recording_test.rs` for the non-streaming path.
#![allow(clippy::field_reassign_with_default)]

use cost::pricing::PricingCatalog;
use cost::{CostState, CostTracker};
use llm_runtime::{TokenUsage, Usage};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop_with_usage,
    message_start_with_usage, message_stop, text_delta, MockStreamingApiClient,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use protocol::SessionId;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tool_api::registry::ToolRegistry;

/// Build a `Usage` with input tokens only (the `message_start` shape on the
/// real Anthropic wire: input + cache counts present, output = 0).
fn start_usage_with_input(input: u64) -> Usage {
    Usage {
        billable_tokens: TokenUsage {
            input,
            output: 0,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Build a `Usage` with output tokens only (the `message_delta` shape on the
/// real Anthropic wire: output present, input/cache = 0).
fn delta_usage_with_output(output: u64) -> Usage {
    Usage {
        billable_tokens: TokenUsage {
            input: 0,
            output,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Construct a streaming orchestrator wired with a `CostTracker` and a
/// scripted `MockStreamingApiClient`. Returns (orch, rx) where rx is the
/// `CostTracker`'s persist channel.
fn make_streaming_orch_with_tracker(
    turns: Vec<Vec<llm_runtime::HistoryEvent>>,
) -> (
    Arc<ConversationOrchestrator>,
    tokio::sync::mpsc::Receiver<CostState>,
    Arc<MockOutputStream>,
) {
    let (tx, rx) = mpsc::channel(8);
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));

    let streaming_api = Arc::new(MockStreamingApiClient::with_turns(turns));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let tools = Arc::new(ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let memory = Arc::new(StaticMemoryProvider::empty());

    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into(); // priced in builtin_reference catalog

    let orch = Arc::new(
        ConversationOrchestrator::new_with_streaming(
            cfg,
            batched,
            streaming_api,
            tools,
            hooks,
            perms,
            output.clone(),
            memory,
            PathBuf::from("/tmp"),
        )
        .with_cost_tracker(tracker),
    );

    (orch, rx, output)
}

/// Streaming turn with known usage → `CostTracker` must receive a snapshot
/// with the correct total_nano_usd (1000 input × 5000 + 500 output × 25000
/// = 17_500_000 nano-USD for claude-opus-4-6, matching the batched-path test).
///
/// Wire shape: `message_start` carries input=1000 (real Anthropic wire);
/// `message_delta` carries output=500 only. The per-field merge must combine
/// them correctly so billing sees input=1000, output=500.
#[tokio::test]
async fn streaming_turn_records_cost_in_tracker() {
    // Real wire: MessageStart carries input=1000, MessageDelta carries output=500.
    let stream = scripted![
        message_start_with_usage("msg_01", "claude-opus-4-6", start_usage_with_input(1_000)),
        content_block_start_text(0),
        text_delta(0, "hi"),
        content_block_stop(0),
        message_delta_stop_with_usage("end_turn", delta_usage_with_output(500)),
        message_stop(),
    ];

    let (orch, mut rx, _output) = make_streaming_orch_with_tracker(vec![stream]);

    orch.run_turn_streaming("hello")
        .await
        .expect("streaming turn ok");

    // CostTracker must have received exactly one snapshot.
    let snap = rx
        .recv()
        .await
        .expect("CostTracker must have received a snapshot from streaming turn");
    // 1000*5000 + 500*25000 = 17_500_000 nano-USD (same as the batched-path test).
    assert_eq!(
        snap.total_nano_usd, 17_500_000,
        "streaming turn must record the same cost as the batched path for identical usage"
    );
}

/// Streaming turn increments `api_calls_recorded` counter (mirrors batched path).
#[tokio::test]
async fn streaming_turn_increments_api_calls_recorded() {
    let stream = scripted![
        message_start_with_usage("msg_01", "claude-opus-4-6", start_usage_with_input(100)),
        content_block_start_text(0),
        text_delta(0, "ok"),
        content_block_stop(0),
        message_delta_stop_with_usage("end_turn", delta_usage_with_output(50)),
        message_stop(),
    ];

    let (orch, mut rx, _output) = make_streaming_orch_with_tracker(vec![stream]);

    orch.run_turn_streaming("hello")
        .await
        .expect("streaming turn ok");

    // Drain channel to confirm recording happened.
    let _snap = rx.recv().await.expect("snapshot received");

    // snapshot_cost_real reads api_calls_recorded; after one streaming turn
    // it must equal 1.
    let cost_snap = orch.snapshot_cost_real().await;
    assert_eq!(
        cost_snap.api_calls, 1,
        "streaming turn must increment api_calls counter"
    );
}
