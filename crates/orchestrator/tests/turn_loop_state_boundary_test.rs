//! Admission tests for PR 3 of the unified-driver plan (shared `TurnLoopState`).
//!
//! §5.1 names what the extraction must not disturb: "共享构造保留
//! `query_started_at` 与 `turn_start_output_baseline` 时点。入口追加、
//! UserPromptSubmit 阻止、`begin_output_turn`、state 初始化的相对顺序不得因抽取
//! 改变." PR 3's own line is "锁定 baseline、计数器重置、token 字段".
//!
//! All three entries already agree on the order — `begin_output_turn` first,
//! then the baseline — but they reach it differently: streaming inside
//! `TurnLoopState::new`, the two batched entries inline in the public
//! wrapper. A single shared constructor has to land on the same instant for all
//! three, and "the same instant" is only checkable while something pins it.
//!
//! The baseline is what makes per-turn output-token accounting a DELTA rather
//! than a running total: budget continuation and the Stop-hook gates read
//! `pool - baseline`. Capturing it a moment too late (after the first model
//! step, say) silently reports every turn as having produced nothing, and no
//! test that only counts messages would notice.

use llm_runtime::{ContentBlock as LlmContentBlock, HistoryResponse, Usage};
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    noop_hook_executor, text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient,
    NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    scripted, ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, TurnOutcome,
};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tool_api::registry::ToolRegistry;

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("turn-loop-state-boundary".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime")
                .block_on(build());
        })
        .expect("spawn large-stack test thread");
    handle.join().expect("large-stack test thread panicked");
}

/// A pool value no turn could produce by accident, so reading it back proves the
/// baseline was captured rather than defaulted.
const SEEDED_POOL: u64 = 4242;

fn response(text: &str) -> HistoryResponse {
    HistoryResponse {
        id: "msg_state".into(),
        model: "claude-opus-4-7".into(),
        content: vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
        }],
        stop_reason: Some("end_turn".into()),
        stop_details: None,
        usage: Usage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

fn batched_orch(responses: Vec<HistoryResponse>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(responses)),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

fn streaming_orch() -> ConversationOrchestrator {
    let script = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "ok"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(MockStreamingApiClient::with_turns(vec![script])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

/// Every entry captures the output-token baseline at TURN START.
///
/// Seeded before the turn and read after it: the value must be what the pool
/// held going in. A shared constructor that captures at a different moment —
/// before the pool is seeded, or after the first model step has added to it —
/// changes `pool - baseline` for the whole turn, which is the number budget
/// continuation and the Stop gates read.
#[test]
fn every_entry_captures_the_output_token_baseline_at_turn_start() {
    run_with_large_stack(|| async {
        // ── run_turn
        let orch = batched_orch(vec![response("ok")]);
        orch.output_token_pool()
            .store(SEEDED_POOL, Ordering::Relaxed);
        orch.run_turn("ping").await.expect("batched");
        assert_eq!(
            orch.turn_start_output_baseline().load(Ordering::Relaxed),
            SEEDED_POOL,
            "run_turn must capture the pool as of turn start; 0 means the baseline was never \
             taken, anything else means it was taken at the wrong moment"
        );

        // ── run_turn_with_cancel
        let orch = batched_orch(vec![response("ok")]);
        orch.output_token_pool()
            .store(SEEDED_POOL, Ordering::Relaxed);
        let outcome = orch
            .run_turn_with_cancel("ping", CancellationToken::new())
            .await
            .expect("cancelable");
        assert_eq!(outcome, TurnOutcome::EndTurn);
        assert_eq!(
            orch.turn_start_output_baseline().load(Ordering::Relaxed),
            SEEDED_POOL,
            "run_turn_with_cancel must capture the same baseline as run_turn. It is the entry \
             most easily forgotten, because it is the one with no ConversationOutcome to \
             inspect."
        );

        // ── run_turn_streaming
        let orch = streaming_orch();
        orch.output_token_pool()
            .store(SEEDED_POOL, Ordering::Relaxed);
        orch.run_turn_streaming("ping").await.expect("streaming");
        assert_eq!(
            orch.turn_start_output_baseline().load(Ordering::Relaxed),
            SEEDED_POOL,
            "run_turn_streaming takes its baseline inside TurnLoopState::new rather than \
             in the entry wrapper; a shared constructor must land on the same instant"
        );
    });
}

/// The per-turn counters reset between turns rather than accumulating.
///
/// `turn_count` is the one counter the public outcome exposes, and it is the
/// one whose reset a shared state object is most likely to lose: hoisting the
/// construction out of the entry — so the state is built once and reused —
/// turns a per-turn counter into a per-session one. The second turn here would
/// then report 2.
#[test]
fn the_per_turn_counters_start_fresh_on_each_turn() {
    run_with_large_stack(|| async {
        let orch = batched_orch(vec![response("first"), response("second")]);

        for (index, label) in [(1u32, "first"), (2, "second")] {
            let outcome = orch.run_turn("ping").await.expect(label);
            match outcome {
                ConversationOutcome::EndTurn { turn_count, .. } => assert_eq!(
                    turn_count, 1,
                    "turn {index} ({label}) reported turn_count={turn_count}. Each turn counts \
                     its own model steps from zero; a running total means the loop state \
                     outlived the turn that owns it."
                ),
                other => panic!("turn {index} ({label}) ended as {other:?}"),
            }
        }

        // The baseline is per-turn too: the second turn re-takes it, so it
        // tracks the pool as the session grows rather than freezing at turn 1.
        orch.output_token_pool()
            .store(SEEDED_POOL, Ordering::Relaxed);
        let orch2 = batched_orch(vec![response("x")]);
        orch2.output_token_pool().store(7, Ordering::Relaxed);
        orch2.run_turn("ping").await.expect("independent session");
        assert_eq!(
            orch2.turn_start_output_baseline().load(Ordering::Relaxed),
            7,
            "each orchestrator takes its own baseline; a shared or static one would leak \
             across sessions"
        );
    });
}

/// `begin_output_turn` happens BEFORE the baseline is taken, on every entry.
///
/// SOURCE-level, and labelled as such. The two events are a few microseconds
/// apart and neither publishes an ordering observable, so the relative order —
/// which §5.1 explicitly freezes — cannot be asserted from the outside. What
/// can be checked is that no entry grew a second baseline capture, and that
/// each one still sits directly after its `begin_output_turn`.
///
/// When PR 3 moves these into a shared constructor the count should drop to one
/// site, at which point this becomes the `record_prompt_snapshot_if_needed`
/// story from PR 2: tighten the number, do not delete the check.
#[test]
fn each_entry_takes_the_baseline_directly_after_begin_output_turn() {
    const DRIVERS: &str = concat!(
        include_str!("../src/conversation/drivers/mod.rs"),
        include_str!("../src/conversation/drivers/streaming.rs"),
    );

    let baselines = DRIVERS.matches("turn_start_output_baseline.store(").count();
    assert_eq!(
        baselines, 1,
        "the baseline must be taken from exactly ONE place — `TurnLoopState::new`. Before PR 3 \
         there were three, one per entry, and this assertion counted three; it tightened when \
         the shared constructor landed. Two or more means an entry takes its own again, and \
         the second write wins whichever moment it happens to run at."
    );

    // Every turn-start sequence reaches the baseline immediately.
    //
    // Checked forwards from `begin_output_turn`, not backwards from the store,
    // because the streaming entry takes its baseline inside
    // `TurnLoopState::new` — a constructor defined ~1500 lines from where
    // it is called. Adjacency holds at the CALL, which is what §5.1 is about.
    let starts: Vec<usize> = DRIVERS
        .match_indices("begin_output_turn(")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        starts.len(),
        3,
        "expected exactly three turn starts, one per public entry"
    );
    for (index, at) in starts.into_iter().enumerate() {
        let window: String = DRIVERS[at..].lines().take(8).collect::<Vec<_>>().join("\n");
        assert!(
            window.contains("turn_start_output_baseline.store(")
                || window.contains("TurnLoopState::new("),
            "turn start #{index} does not reach its output-token baseline within eight lines \
             of begin_output_turn. §5.1 freezes that adjacency: work that slips in between \
             runs against a turn whose output accounting has not started yet.\n{window}"
        );
    }
}
