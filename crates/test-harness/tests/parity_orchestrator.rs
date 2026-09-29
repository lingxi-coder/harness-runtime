//! Parity: end-to-end turn loop locks.
//!
//! Locks the `ConversationOrchestrator` turn loop behavior surface for
//! v0.6.0: single-turn completion, max-turns cap, and cancellation.
//! Uses `MockApiClient` from `orchestrator::test_support`.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-14-release-v0.6.0.md` Task 2.
#![allow(clippy::field_reassign_with_default)]

use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, OrchestratorError,
    TurnOutcome,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ============================================================================
// Fixture types
// ============================================================================

#[derive(Debug, Deserialize)]
struct Fixture {
    scenarios: Vec<Scenario>,
    telemetry_invariants: TelemetryInvariants,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    user_prompt: Option<String>,
    #[serde(default)]
    expected_turn_count: Option<u32>,
    #[serde(default)]
    expected_session_messages: Option<usize>,
    #[serde(default)]
    expected_outcome: Option<String>,
    #[serde(default)]
    max_turns_override: Option<u32>,
    #[serde(default)]
    pre_cancel: bool,
    // M6-06 cost_after_one_turn scenario fields. Unused at the loader
    // level — the new `parity_cost_after_one_turn` test below wires the
    // assertion directly. These #[serde(default)] fields just confirm the
    // fixture parses cleanly.
    #[serde(default)]
    #[allow(dead_code)]
    model: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    response_usage: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    expected_cost_after_turn: Option<serde_json::Value>,
    // M6-08 force_compact_50_messages scenario fields. The dedicated
    // `parity_force_compact_50_messages` test below wires the assertion
    // directly; these #[serde(default)] fields just confirm the fixture
    // parses cleanly.
    #[serde(default)]
    seed_history_size: Option<usize>,
    #[serde(default)]
    expected_messages_before: Option<u32>,
    #[serde(default)]
    expected_messages_after_max: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    expected_marker_present: Option<bool>,
    #[serde(default)]
    expected_marker_prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelemetryInvariants {
    events_fired_per_normal_turn: Vec<String>,
}

const FIXTURE: &str = include_str!("../src/parity/fixtures/parity_orchestrator_turn_loop.json");

fn load() -> Fixture {
    serde_json::from_str(FIXTURE).expect("fixture parse")
}

// ============================================================================
// Helpers
// ============================================================================

fn build_orchestrator_with_api(
    api: Arc<MockApiClient>,
    max_turns: Option<u32>,
) -> ConversationOrchestrator {
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let output = Arc::new(MockOutputStream::new());
    let mut cfg = OrchestratorConfig::default();
    if let Some(mt) = max_turns {
        cfg.max_turns = mt;
    }
    ConversationOrchestrator::new(
        cfg,
        api,
        tools,
        hooks,
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

// ============================================================================
// T2 — scenario: single_turn_no_tools
// ============================================================================

#[tokio::test]
async fn single_turn_no_tools_completes_with_end_turn() {
    let f = load();
    let s = f
        .scenarios
        .iter()
        .find(|s| s.name == "single_turn_no_tools")
        .unwrap();

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "Hello!".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let orch = build_orchestrator_with_api(api.clone(), None);

    let outcome = orch
        .run_turn(s.user_prompt.as_deref().unwrap())
        .await
        .expect("turn must succeed");

    match outcome {
        ConversationOutcome::EndTurn { turn_count, .. } => {
            assert_eq!(
                turn_count,
                s.expected_turn_count.unwrap_or(1),
                "turn_count mismatch for scenario {}",
                s.name
            );
        }
        _ => panic!("expected EndTurn for scenario {}", s.name),
    }

    // The session should have user + assistant = 2 messages.
    let captured = api.captured_msgs().await;
    assert_eq!(
        captured.len(),
        1,
        "exactly one API call for single-turn scenario"
    );
    // R-P1: every outgoing API call LEADS with the additional-context
    // `<system-reminder>` meta (claudeMd/gitStatus/currentDate — currentDate is
    // unconditional, so the meta is always present), so the API receives that
    // meta and TRAILS with the transient total-tokens reminder around the
    // user message(s), before the assistant is appended.
    let user_msgs_before_assistant = s.expected_session_messages.unwrap_or(2) - 1;
    assert_eq!(
        captured[0].len(),
        user_msgs_before_assistant + 2,
        "API receives additional-context + user messages + total-tokens reminder"
    );

    assert_eq!(s.expected_outcome.as_deref(), Some("EndTurn"));
}

// ============================================================================
// T2 — scenario: max_turns_cap
// ============================================================================

#[tokio::test]
async fn max_turns_cap_returns_max_turns_reached_error() {
    let f = load();
    let s = f
        .scenarios
        .iter()
        .find(|s| s.name == "max_turns_cap")
        .unwrap();

    let max = s.max_turns_override.unwrap_or(2);

    // Provide more responses than `max` so the loop keeps going until the TURN
    // CAP fires (not until the queue empties). Each response uses a non-
    // `end_turn` stop_reason (`max_tokens`) so there is no natural end — the
    // loop runs until `turn_count >= max`, exactly like the orchestrator's
    // `never_ending_loop_aborts_with_max_turns_reached` unit test.
    let responses: Vec<_> = (0..max + 2)
        .map(|_| {
            mock_message_response(
                vec![LlmContentBlock::Text {
                    text: "still going".into(),
                    cache_control: None,
                }],
                Some("max_tokens"),
            )
        })
        .collect();

    let api = Arc::new(MockApiClient::new(responses));
    // A POSITIVE `max` imposes the cap. (`0` now means UNBOUNDED — parity with
    // claude-code's optional `maxTurns` — so the cap must be a non-zero value;
    // see `OrchestratorConfig::max_turns`.)
    let orch = build_orchestrator_with_api(api, Some(max));

    let err = orch
        .run_turn(s.user_prompt.as_deref().unwrap())
        .await
        .expect_err("should fail with MaxTurnsReached when the turn cap is hit");

    assert!(
        matches!(err, OrchestratorError::MaxTurnsReached { .. }),
        "expected MaxTurnsReached but got {err:?}"
    );
    assert_eq!(s.expected_outcome.as_deref(), Some("MaxTurnsReached"));
}

// ============================================================================
// T2 — scenario: cancel_before_first_turn  (uses run_turn_with_cancel)
// ============================================================================

#[tokio::test]
async fn cancel_before_first_turn_returns_cancelled() {
    let f = load();
    let s = f
        .scenarios
        .iter()
        .find(|s| s.name == "cancel_before_first_turn")
        .unwrap();

    assert!(s.pre_cancel, "scenario must have pre_cancel = true");

    let api = Arc::new(MockApiClient::new(vec![]));
    let orch = build_orchestrator_with_api(api, None);

    // Pre-cancel the token before calling run_turn_with_cancel.
    let cancel = CancellationToken::new();
    cancel.cancel();

    let outcome = orch
        .run_turn_with_cancel(s.user_prompt.as_deref().unwrap(), cancel)
        .await
        .expect("cancelled turn must return Ok(Cancelled), not Err");

    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert_eq!(s.expected_outcome.as_deref(), Some("Cancelled"));
}

// ============================================================================
// T2 — telemetry invariants: fixture names are registered in ALL_EVENT_NAMES
// ============================================================================

#[test]
fn telemetry_invariants_events_are_registered() {
    let f = load();
    let registered: std::collections::HashSet<&&str> =
        telemetry::tengu::ALL_EVENT_NAMES.iter().collect();
    for name in &f.telemetry_invariants.events_fired_per_normal_turn {
        assert!(
            registered.contains(&name.as_str()),
            "event {name:?} listed in parity fixture but not registered in ALL_EVENT_NAMES"
        );
    }
}

// ============================================================================
// T2 — fixture meta: scenario names are unique and expected_outcomes are valid
// ============================================================================

#[test]
fn fixture_scenarios_names_unique_and_outcomes_valid() {
    let f = load();
    let valid_outcomes = ["EndTurn", "MaxTurnsReached", "Cancelled"];
    let mut seen = std::collections::HashSet::new();
    for s in &f.scenarios {
        assert!(
            seen.insert(s.name.clone()),
            "duplicate scenario name {}",
            s.name
        );
        // M6-06 cost_after_one_turn scenario has no expected_outcome — its
        // assertion is the dedicated `parity_cost_after_one_turn` test below.
        if let Some(outcome) = s.expected_outcome.as_deref() {
            assert!(
                valid_outcomes.contains(&outcome),
                "scenario {}: unknown expected_outcome {:?}",
                s.name,
                outcome
            );
        }
    }
}

// ============================================================================
// M6-06 — scenario: cost_after_one_turn (real CostTracker wiring)
// ============================================================================

#[tokio::test]
async fn parity_cost_after_one_turn() {
    use cost::pricing::PricingCatalog;
    use cost::CostTracker;
    use platform_api::OrchestratorHandle;
    use protocol::SessionId;
    use tokio::sync::mpsc;

    let response = llm_runtime::HistoryResponse {
        id: "msg_mock".into(),
        model: "claude-opus-4-6".into(),
        // A realistic end_turn response carries visible text. (An empty-content
        // response would trip the #78 thinking-only nudge — which is faithful:
        // claude-code also nudges an end_turn with no visible text — so the mock
        // must produce visible text for a single-turn completion.)
        content: vec![LlmContentBlock::Text {
            text: "Done.".into(),
            cache_control: None,
        }],
        stop_reason: Some("end_turn".into()),
        stop_details: None,
        // COST.3/5: new UsageApi fields default to None (no web-search /
        // non-fast) → base pricing, so this fixture's asserted cost is
        // unchanged.
        usage: llm_runtime::Usage {
            billable_tokens: llm_runtime::TokenUsage {
                input: 1_000,
                output: 500,
                ..Default::default()
            },
            ..Default::default()
        },
        cost: None,
        provider_metadata: serde_json::Value::Null,
    };
    let api = Arc::new(MockApiClient::new(vec![response]));
    let (tx, _rx) = mpsc::channel(64);
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));
    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into();
    let orch = Arc::new(
        ConversationOrchestrator::new(
            cfg,
            api,
            Arc::new(tool_api::registry::ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_cost_tracker(tracker),
    );
    orch.run_turn("hi").await.unwrap();

    let snap = orch.snapshot_cost().await;
    assert_eq!(snap.total_nano_usd, 17_500_000);
    assert!((snap.total_usd - 0.0175).abs() < 1e-9);
    assert_eq!(snap.input_tokens, 1_000);
    assert_eq!(snap.output_tokens, 500);
    assert_eq!(snap.api_calls, 1);
}

// ============================================================================
// M6-08 — scenario: force_compact_50_messages (real CompactionOrchestrator)
// ============================================================================

#[tokio::test]
async fn parity_force_compact_50_messages() {
    use compaction::CompactionOrchestrator;
    use platform_api::OrchestratorHandle;
    use protocol::{ConversationMessage, MessageId};

    // Drive the assertion from the fixture so the scenario fields are
    // load-bearing (matches the cost_after_one_turn convention).
    let f = load();
    let s = f
        .scenarios
        .iter()
        .find(|s| s.name == "force_compact_50_messages")
        .expect("force_compact_50_messages scenario present");
    let seed = s.seed_history_size.expect("seed_history_size");
    let expected_before = s
        .expected_messages_before
        .expect("expected_messages_before");
    let after_max = s
        .expected_messages_after_max
        .expect("expected_messages_after_max");
    let marker_prefix = s
        .expected_marker_prefix
        .clone()
        .expect("expected_marker_prefix");

    // Manual `/compact` (force_compact) HARD-FAILS without a wired
    // side-query summarizer (an explicit command must never fake success),
    // so the parity driver wires a stub client returning a canned
    // `<summary>` — the history collapse and boundary marker are real.
    struct StubCompactClient;
    #[async_trait::async_trait]
    impl sidequery::SideQueryClient for StubCompactClient {
        async fn query(
            &self,
            _request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            Ok(sidequery::SideQueryResponse {
                text: Some("<summary>parity stub summary</summary>".into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }
    let cache_slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let forked_runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(Arc::new(StubCompactClient), "test-compact-model".into()),
    );

    let orch = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        // Tiny threshold (100 tokens ≈ 400 text chars) so the compaction
        // layer reliably fires regardless of per-message body length —
        // estimate_tokens_for_range is text_content().len()/4 per message,
        // so 50 short messages clear 100 tokens with margin.
        .with_cache_safe_slot(cache_slot.clone())
        .with_compaction(Arc::new(CompactionOrchestrator::with_autocompactor(
            // The SAME slot the orchestrator seeds (`save_cache_safe_params`)
            // — the forked summarizer reads its request params from it.
            compaction::Autocompactor::with_forked_runner(forked_runner, cache_slot),
            100,
        ))),
    );

    // Seed `seed` alternating user/assistant messages so manual compaction has
    // at least two complete API rounds, matching the live transcript shape.
    {
        let session = orch.session();
        let mut hist = session.lock().await;
        for i in 0..seed {
            if i % 2 == 0 {
                hist.history.push(ConversationMessage::user(
                    MessageId::new(),
                    format!("turn-{i} padding to push token count past the autocompact threshold"),
                ));
            } else {
                hist.history.push(ConversationMessage::Assistant {
                    id: MessageId::new(),
                    content: vec![protocol::ContentBlock::Text {
                        text: format!("reply-{i}"),
                    }],
                    stop_reason: Some("end_turn".into()),
                });
            }
        }
    }

    let summary = orch.force_compact().await.expect("force_compact ok");
    // COUNTS are real (asserted); the summary body is the M3 stub.
    assert_eq!(summary.messages_before, expected_before);
    assert!(
        summary.messages_after <= after_max,
        "messages_after={} must be <= {after_max} (collapse happened)",
        summary.messages_after
    );

    let session = orch.session();
    let hist = session.lock().await;
    // COMPACT.1: the compact boundary marker now LEADS the post-compact history
    // (TS `buildPostCompactMessages` order `[boundaryMarker, ...summary, ...]`),
    // so assert on `first()` rather than `last()`.
    let first = hist
        .history
        .first()
        .expect("history non-empty after compact");
    match first {
        ConversationMessage::System { content, .. } => {
            assert!(
                content.starts_with(&marker_prefix),
                "marker prefix {marker_prefix:?} missing; got: {content}"
            );
        }
        other => panic!("expected System marker; got {other:?}"),
    }
}
