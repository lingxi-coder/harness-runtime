//!
//! Verifies that:
//! - A 50-message history compacts to fewer than 50 messages and the
//!   trait surface reports real numbers (`messages_before == 50`,
//!   `messages_after < 50`).
//! - A compaction failure leaves history untouched.
//! - A cancelled compaction leaves history untouched.
//! - The post-compact history carries the `[Compacted N → M]` marker as
//!   its last message, so the next turn's system-prompt assembly sees
//!   the compaction transition.
//! - Five consecutive `force_compact` calls do not panic / leak.
use llm_runtime::ContentBlock as LlmContentBlock;

use compaction::CompactionOrchestrator;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use platform_api::OrchestratorHandle;
use protocol::{ConversationMessage, MessageId};
use std::sync::Arc;

struct CaptureCompactClient {
    seen: std::sync::Mutex<Option<sidequery::SideQueryRequest>>,
}

#[async_trait::async_trait]
impl sidequery::SideQueryClient for CaptureCompactClient {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        *self.seen.lock().unwrap() = Some(request);
        Ok(sidequery::SideQueryResponse {
            text: Some("<summary>captured</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn wired_compaction() -> (
    Arc<CompactionOrchestrator>,
    Arc<sidequery::CacheSafeParamsSlot>,
) {
    let client = Arc::new(CaptureCompactClient {
        seen: std::sync::Mutex::new(None),
    });
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(client, "test-compact-model".into()),
    );
    (
        Arc::new(CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
            1_000,
        )),
        slot,
    )
}

fn make_orch() -> Arc<ConversationOrchestrator> {
    let api = Arc::new(MockApiClient::new(vec![]));
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());
    let (compactor, slot) = wired_compaction();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output,
        memory,
        std::env::temp_dir(),
    )
    .with_cache_safe_slot(slot)
    .with_compaction(compactor);
    Arc::new(orch)
}

async fn seed_history(orch: &ConversationOrchestrator, n: usize) {
    let session = orch.session();
    let mut s = session.lock().await;
    for i in 0..n {
        if i % 2 == 0 {
            s.history.push(ConversationMessage::user(
                MessageId::new(),
                format!("turn-{i} body padded with filler text to push token count up beyond autocompact threshold"),
            ));
        } else {
            s.history.push(ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![protocol::ContentBlock::Text {
                    text: format!("reply-{i} padded with enough detail for compaction"),
                }],
                stop_reason: Some("end_turn".into()),
            });
        }
    }
}

async fn history_len(orch: &ConversationOrchestrator) -> usize {
    let session = orch.session();
    let s = session.lock().await;
    s.history.len()
}

#[tokio::test]
async fn with_compaction_builder_stores_compactor() {
    let orch = make_orch();
    assert!(
        orch.has_compaction(),
        "with_compaction builder did not store the compactor"
    );
}

#[tokio::test]
async fn unwired_manual_compact_is_an_error_not_fake_success() {
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let err = orch
        .force_compact()
        .await
        .expect_err("missing compactor must not report a zero-delta success");
    assert!(err.to_string().contains("compaction unavailable"));
}

#[tokio::test]
async fn compacts_50_message_history() {
    let orch = make_orch();
    seed_history(&orch, 50).await;
    let usage_before = orch.context_usage_snapshot().await;

    let summary = orch.force_compact().await.expect("force_compact ok");
    let usage_after = orch.context_usage_snapshot().await;

    assert_eq!(summary.messages_before, 50);
    assert!(
        summary.messages_after < 50,
        "messages_after={} must be <50 to count as compacted",
        summary.messages_after
    );
    assert!(
        usage_after.live_context_tokens < usage_before.live_context_tokens,
        "live context must fall after compact: before={}, after={}",
        usage_before.live_context_tokens,
        usage_after.live_context_tokens
    );
    assert_eq!(
        usage_after.cumulative_cost, usage_before.cumulative_cost,
        "compaction must not reset cumulative session cost"
    );
}

#[tokio::test]
async fn manual_compact_seeds_empty_resume_slot_and_forwards_focus() {
    let config_home = tempfile::tempdir().expect("temp config home");
    let client = Arc::new(CaptureCompactClient {
        seen: std::sync::Mutex::new(None),
    });
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(client.clone(), "startup-model".into()),
    );
    let compactor = Arc::new(CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
        u64::MAX,
    ));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_config_home(config_home.path().to_path_buf())
    .with_cache_safe_slot(slot)
    .with_compaction(compactor);
    seed_history(&orch, 4).await;

    orch.force_compact_with_instructions_and_cancel(
        Some("focus on the Rust fixes"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("manual compact should work before any live API turn");

    let request = client.seen.lock().unwrap().clone().expect("LLM was called");
    assert_eq!(request.model, OrchestratorConfig::default().model);
    let texts: Vec<String> = request
        .messages
        .iter()
        .map(ConversationMessage::text_content)
        .collect();
    assert!(texts.iter().any(|text| text.contains("reply-1")));
    assert!(texts.iter().any(|text| text.contains("turn-2")));
    // Binary-verified (`Juy → mXi → Nto`, `s = 1`): the manual path preserves
    // the LAST API-round group verbatim — `[turn-0],[reply-1,turn-2],[reply-3]`
    // → the newest reply is the preserved tail and must NOT reach the
    // summarizer. (An earlier revision asserted the full-history
    // `messagesToKeep: []` shape here; that is the auto-only `Pto` path.)
    assert!(
        !texts.iter().any(|text| text.contains("reply-3")),
        "the preserved tail (newest reply) never reaches the summarizer"
    );
    assert!(texts
        .last()
        .is_some_and(|text| text.contains("Additional Instructions:\nfocus on the Rust fixes")));

    let summary_text = {
        let session = orch.session();
        let state = session.lock().await;
        state
            .history
            .iter()
            .map(ConversationMessage::text_content)
            .find(|text| text.contains("Summary:\ncaptured"))
            .expect("compact summary in post-compact history")
    };
    assert!(
        summary_text.contains("read the full transcript at:"),
        "the continuation must retain Claude Code's transcript recovery pointer"
    );
    assert!(
        summary_text.contains(config_home.path().to_string_lossy().as_ref()),
        "the recovery pointer must use this session's configured transcript root"
    );

    // The preserved tail rides verbatim in the post-compact history.
    {
        let session = orch.session();
        let state = session.lock().await;
        assert!(
            state
                .history
                .iter()
                .any(|m| m.text_content().contains("reply-3")),
            "the newest reply is preserved verbatim after the summary"
        );
    }
}

#[tokio::test]
async fn post_compact_summary_is_visible_to_next_turn() {
    let orch = make_orch();
    seed_history(&orch, 20).await;

    orch.force_compact().await.unwrap();

    // Inspect history: must contain the boundary marker as the FIRST
    // message (COMPACT.1: marker leads post-compact history, matching TS
    // buildPostCompactMessages), AND at least one following message that
    // is the summary (length-checked: compactor produces ≥1 message + our
    // marker).
    let session = orch.session();
    let s = session.lock().await;
    let first = s.history.first().expect("history non-empty after compact");
    let ConversationMessage::System { content, .. } = first else {
        panic!("expected System message; got {first:?}");
    };
    assert_eq!(
        content, "Conversation compacted",
        "CSM.4 boundary sentinel missing; got: {content}"
    );

    // The summary message produced by the compactor sits AFTER the
    // marker (COMPACT.1). With the M3 stub Autocompactor this is
    // `[stub-summary attempt=0; messages=20]`.
    assert!(
        s.history.len() >= 2,
        "expected ≥2 messages (marker + summary), got {}",
        s.history.len()
    );
}

#[tokio::test]
async fn cancel_during_compaction_leaves_history_unchanged() {
    use tokio_util::sync::CancellationToken;

    let orch = make_orch();
    seed_history(&orch, 30).await;
    let len_before = history_len(&orch).await;

    let token = CancellationToken::new();
    token.cancel(); // already cancelled — first poll exits

    let err = orch
        .force_compact_with_cancel(token)
        .await
        .expect_err("cancelled must error");
    assert!(
        err.to_string().contains("Compaction canceled."),
        "got: {err}"
    );

    let len_after = history_len(&orch).await;
    assert_eq!(len_before, len_after);
}

/// Build a `CompactionOrchestrator` whose `Autocompactor` is wired with
/// an empty `CacheSafeParamsSlot`. The autocompact layer's
/// `slot.get_last().await` returns `None`, surfacing
/// `CompactionError::Internal("no cache-safe params")`.
fn errored_compactor() -> Arc<CompactionOrchestrator> {
    use compaction::autocompact::Autocompactor;
    use sidequery::{CacheSafeParamsSlot, ForkedAgentRunner};

    let runner = Arc::new(ForkedAgentRunner::new());
    let slot = Arc::new(CacheSafeParamsSlot::new()); // empty — triggers Internal err

    // Threshold 1 token → autocompact ALWAYS fires.
    let orch = CompactionOrchestrator::with_autocompactor(
        Autocompactor::with_forked_runner(runner, slot),
        1,
    );
    Arc::new(orch)
}

#[tokio::test]
async fn failure_leaves_history_unchanged() {
    let api = Arc::new(MockApiClient::new(vec![]));
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output,
        memory,
        std::env::temp_dir(),
    )
    .with_compaction(errored_compactor());

    // Seed 5 padded messages so estimate_tokens_for_range exceeds the
    // 1-token threshold and autocompact actually fires.
    {
        let session = orch.session();
        let mut s = session.lock().await;
        for i in 0..6 {
            if i % 2 == 0 {
                s.history.push(ConversationMessage::user(
                    MessageId::new(),
                    format!("m{i} body padded with filler text to ensure token estimate > 1"),
                ));
            } else {
                s.history.push(ConversationMessage::Assistant {
                    id: MessageId::new(),
                    content: vec![protocol::ContentBlock::Text {
                        text: format!("reply-{i}"),
                    }],
                    stop_reason: Some("end_turn".into()),
                });
            }
        }
    }
    let len_before = {
        let session = orch.session();
        let s = session.lock().await;
        s.history.len()
    };

    let err = orch.force_compact().await.expect_err("must fail");
    let s = err.to_string();
    assert!(
        s.contains("Error during compaction: no forked summarizer wired"),
        "got: {s}"
    );

    let len_after = {
        let session = orch.session();
        let s = session.lock().await;
        s.history.len()
    };
    assert_eq!(
        len_before, len_after,
        "history must be untouched on failure"
    );
}

#[tokio::test]
async fn five_consecutive_force_compact_calls_do_not_explode() {
    let orch = make_orch();
    seed_history(&orch, 30).await;

    orch.force_compact().await.expect("first compact succeeds");
    for i in 1..5 {
        let err = orch
            .force_compact()
            .await
            .expect_err("already-compacted short history must reject");
        assert!(
            err.to_string().contains("Not enough messages to compact."),
            "iter {i}: {err}"
        );
    }

    // No assertion on final length — the stub autocompact collapses to
    // 1 + marker on each pass; we just confirm no panics / no leaks
    // (validated implicitly by `cargo test` finishing).
}

// ============================================================================
// M6-08 Task 14 — Compaction Safety Gate (HARD GATE)
//
// One test exercising the three gate assertions end-to-end:
//   1. messages_after < messages_before (collapse happened).
//   2. Round-trip integrity: the post-compaction history is still a VALID
//      message sequence — no dangling tool_use without tool_result, the
//      [Compacted] System marker is the well-formed tail, and the
//      summary message precedes it.
//   3. The NEXT TURN after compaction still runs: the orchestrator
//      accepts the compacted history and `run_turn` completes with
//      EndTurn (no panic, no corruption).
//
// Asserts on COUNTS + structural validity (real), NOT on the M3 stub
// summary body.
// ============================================================================

/// Returns `true` if `history` is a structurally valid message sequence:
/// every assistant `ToolUse` block has a matching `ToolResult` in a later
/// user message, and no orphan `ToolResult` precedes its `ToolUse`.
fn history_is_valid(history: &[ConversationMessage]) -> bool {
    use protocol::{ContentBlock, ToolUseId};
    use std::collections::HashSet;

    let mut open_tool_uses: HashSet<ToolUseId> = HashSet::new();
    let mut satisfied: HashSet<ToolUseId> = HashSet::new();
    for msg in history {
        match msg {
            ConversationMessage::Assistant { content, .. } => {
                for b in content {
                    if let ContentBlock::ToolUse { id, .. } = b {
                        open_tool_uses.insert(id.clone());
                    }
                }
            }
            ConversationMessage::User { content, .. } => {
                for b in content {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = b {
                        // A tool_result must reference a tool_use we've
                        // already seen — orphan results are corruption.
                        if !open_tool_uses.contains(tool_use_id) {
                            return false;
                        }
                        satisfied.insert(tool_use_id.clone());
                    }
                }
            }
            ConversationMessage::System { .. } => {}
        }
    }
    // Every tool_use must eventually be satisfied by a tool_result.
    open_tool_uses.is_subset(&satisfied)
}

#[tokio::test]
async fn compaction_safety_gate() {
    use llm_runtime::{ExecutionUsage as Usage, HistoryResponse};

    // Scripted end_turn response so the post-compaction turn can run.
    let response = HistoryResponse {
        id: "msg_gate".into(),
        model: "claude-opus-4-7".into(),
        content: vec![LlmContentBlock::Text {
            text: "ack".into(),
            cache_control: None,
        }],
        stop_reason: Some("end_turn".into()),
        stop_details: None,
        usage: Usage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    };
    let api = Arc::new(MockApiClient::new(vec![response]));
    let (compactor, slot) = wired_compaction();
    let orch = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_cache_safe_slot(slot)
        .with_compaction(compactor),
    );

    seed_history(&orch, 50).await;

    // --- Assertion 1: collapse happened ---
    let summary = orch.force_compact().await.expect("force_compact ok");
    assert_eq!(summary.messages_before, 50, "before count must be 50");
    assert!(
        summary.messages_after < summary.messages_before,
        "GATE#1 FAIL: messages_after={} not < messages_before={}",
        summary.messages_after,
        summary.messages_before
    );

    // --- Assertion 2: round-trip integrity ---
    {
        let session = orch.session();
        let s = session.lock().await;
        assert!(!s.history.is_empty(), "GATE#2 FAIL: empty history");
        assert!(
            history_is_valid(&s.history),
            "GATE#2 FAIL: post-compaction history is not a valid message sequence"
        );
        let first = s.history.first().unwrap();
        match first {
            ConversationMessage::System { content, .. } => {
                // CSM.4: the boundary is now the TS "Conversation compacted"
                // sentinel; the pre/post counts live in the (sidecar) metadata,
                // no longer inline in the marker. COMPACT.1: the marker now
                // LEADS the post-compact history.
                assert_eq!(
                    content, "Conversation compacted",
                    "GATE#2 FAIL: boundary sentinel missing; got: {content}"
                );
            }
            other => panic!("GATE#2 FAIL: expected System marker head; got {other:?}"),
        }
        // The tail message is valid (history is non-empty).
        assert!(
            s.history.last().is_some(),
            "GATE#2 FAIL: tail message missing"
        );
    }

    // --- Assertion 3: the next turn still runs against compacted history ---
    let outcome = orch
        .run_turn("continue after compaction")
        .await
        .expect("GATE#3 FAIL: run_turn errored on compacted history");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "GATE#3 FAIL: expected EndTurn after compaction; got {outcome:?}"
    );
}

#[derive(Default)]
struct CompactLifecycleOutput(std::sync::Mutex<Vec<String>>);

#[async_trait::async_trait]
impl platform_api::OutputStream for CompactLifecycleOutput {
    async fn emit_text(&self, _text: &str) {}
    async fn emit_tool_call(
        &self,
        _id: &protocol::ToolUseId,
        _tool: &str,
        _input: &serde_json::Value,
    ) {
    }
    async fn emit_tool_result(
        &self,
        _id: &protocol::ToolUseId,
        _tool: &str,
        _model_text: &str,
        _result: &serde_json::Value,
    ) {
    }
    async fn emit_end_turn(&self, _reason: &str, _cost: &platform_api::CostSnapshot) {}
    async fn emit_compaction_started(&self) {
        self.0.lock().unwrap().push("started".into());
    }
    async fn emit_compaction_phase(&self, phase: &str) {
        self.0.lock().unwrap().push(phase.into());
    }
    async fn emit_compaction_finished(&self, error: Option<&str>) {
        self.0
            .lock()
            .unwrap()
            .push(error.map_or_else(|| "success".into(), |error| format!("failed:{error}")));
    }
    async fn emit_compact_boundary(
        &self,
        _uuid: &str,
        _metadata: &protocol::CompactBoundaryMetadata,
    ) {
        self.0.lock().unwrap().push("boundary".into());
    }
}

struct LifecycleSummaryClient(&'static str, Option<tokio_util::sync::CancellationToken>);

#[async_trait::async_trait]
impl sidequery::SideQueryClient for LifecycleSummaryClient {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        if let Some(cancel) = &self.1 {
            cancel.cancel();
        }
        Ok(sidequery::SideQueryResponse {
            text: Some(self.0.into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

#[tokio::test]
async fn manual_status_lifecycle_matches_success_empty_and_too_short_oracles() {
    for (count, summary, cancel_after_summary, expected) in [
        (0, "ok", false, Vec::<&str>::new()),
        (
            2,
            "ok",
            false,
            vec![
                "started",
                "summarizing",
                "failed:Not enough messages to compact.",
            ],
        ),
        (
            4,
            "\u{feff} \n\t",
            false,
            vec![
                "started",
                "summarizing",
                "failed:Error during compaction: summarization produced empty response",
            ],
        ),
        (
            4,
            "<summary>ok</summary>",
            false,
            vec!["started", "summarizing", "restoring", "success", "boundary"],
        ),
        (
            4,
            "<summary>ok</summary>",
            true,
            vec!["started", "summarizing", "failed:Compaction canceled."],
        ),
    ] {
        let cancel = tokio_util::sync::CancellationToken::new();
        let output = Arc::new(CompactLifecycleOutput::default());
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(sidequery::ForkedAgentRunner::new().with_side_query_client(
            Arc::new(LifecycleSummaryClient(
                summary,
                cancel_after_summary.then(|| cancel.clone()),
            )),
            "test".into(),
        ));
        let compact = Arc::new(CompactionOrchestrator::with_autocompactor(
            compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
            u64::MAX,
        ));
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_cache_safe_slot(slot)
        .with_compaction(compact);
        seed_history(&orch, count).await;
        let before = orch.session().lock().await.history.clone();
        let result = orch.force_compact_with_cancel(cancel).await;
        assert_eq!(
            *output.0.lock().unwrap(),
            expected,
            "history length {count}, response {summary:?}"
        );
        if result.is_err() {
            assert_eq!(orch.session().lock().await.history, before);
        }
    }
}

// ===== CMP-1: `/rewind` → Summarize from / up to here (oracle `zir`) =========

/// Read back the live history as plain text, in order.
async fn history_texts(orch: &ConversationOrchestrator) -> Vec<String> {
    let session = orch.session();
    let s = session.lock().await;
    s.history
        .iter()
        .map(protocol::ConversationMessage::text_content)
        .collect()
}

/// Seed a short, identifiable conversation and return the uuid of message `at`.
async fn seed_marked(orch: &ConversationOrchestrator, n: usize, at: usize) -> String {
    let session = orch.session();
    let mut s = session.lock().await;
    let mut chosen = String::new();
    for i in 0..n {
        let message = if i % 2 == 0 {
            ConversationMessage::user(MessageId::new(), format!("USER-{i}"))
        } else {
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![protocol::ContentBlock::Text {
                    text: format!("ASSISTANT-{i}"),
                }],
                stop_reason: Some("end_turn".into()),
            }
        };
        if i == at {
            chosen = message.id().to_string();
        }
        s.history.push(message);
    }
    chosen
}

/// 🚨 The property the whole feature turns on: which half survives, and where
/// the summary lands relative to it.
///
/// Both directions run the same code and both produce a plausible history, so
/// a swapped direction is invisible without asserting the actual message order.
#[tokio::test]
async fn summarize_up_to_replaces_the_past_and_keeps_the_present() {
    let orch = make_orch();
    let chosen = seed_marked(&orch, 6, 3).await;

    orch.summarize_at(
        &chosen,
        None,
        platform_api::SummarizeDirection::UpTo,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("summarize_at ok");

    let texts = history_texts(&orch).await;
    // [boundary, summary, kept…]
    assert!(
        texts[1].contains("Summary:"),
        "the summary must lead the kept messages: {texts:?}"
    );
    let kept: Vec<&String> = texts
        .iter()
        .filter(|t| t.starts_with("ASSISTANT-3"))
        .collect();
    assert_eq!(
        kept.len(),
        1,
        "the chosen message is KEPT by up_to: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.starts_with("USER-0")),
        "everything before the chosen message is summarized away: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.starts_with("ASSISTANT-5")),
        "…and everything after it survives: {texts:?}"
    );
}

#[tokio::test]
async fn summarize_from_keeps_the_past_and_replaces_the_present() {
    let orch = make_orch();
    let chosen = seed_marked(&orch, 6, 3).await;

    orch.summarize_at(
        &chosen,
        None,
        platform_api::SummarizeDirection::From,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("summarize_at ok");

    let texts = history_texts(&orch).await;
    assert!(
        texts.iter().any(|t| t.starts_with("USER-0")),
        "the earlier messages survive `from`: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.starts_with("ASSISTANT-5")),
        "everything from the chosen message on is summarized away: {texts:?}"
    );
    let summary_at = texts
        .iter()
        .position(|t| t.contains("Summary:"))
        .expect("a summary message");
    let last_kept = texts
        .iter()
        .rposition(|t| t.starts_with("USER-") || t.starts_with("ASSISTANT-"))
        .expect("a kept message");
    assert!(
        summary_at > last_kept,
        "`from`'s summary must come AFTER the messages it follows, not before \
         them — otherwise the model reads the conversation backwards: {texts:?}"
    );
}

/// The byte-exact guards, and that they are direction-specific.
#[tokio::test]
async fn an_edge_selection_reports_the_oracles_sentence() {
    let orch = make_orch();
    let first = seed_marked(&orch, 4, 0).await;

    let err = orch
        .summarize_at(
            &first,
            None,
            platform_api::SummarizeDirection::UpTo,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("nothing precedes the first message");
    assert!(
        err.to_string()
            .contains("Nothing to summarize before the selected message."),
        "got {err}"
    );

    // …and the same selection is fine in the other direction.
    orch.summarize_at(
        &first,
        None,
        platform_api::SummarizeDirection::From,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("everything after the first message is summarizable");
}

#[tokio::test]
async fn an_unknown_message_uuid_is_reported_not_guessed() {
    let orch = make_orch();
    seed_marked(&orch, 4, 0).await;
    let err = orch
        .summarize_at(
            "not-a-message-in-this-conversation",
            None,
            platform_api::SummarizeDirection::From,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("an unknown uuid must not silently summarize something else");
    assert!(err.to_string().contains("Message not found."), "got {err}");
}

/// The 5-arg boundary (`mne(trigger, preTokens, lastUuid, userContext,
/// messagesSummarized)`) that upstream sets ONLY on this path.
#[tokio::test]
async fn the_boundary_records_the_user_context_and_the_count() {
    let orch = make_orch();
    let chosen = seed_marked(&orch, 6, 3).await;

    orch.summarize_at(
        &chosen,
        Some("  keep the parser notes  "),
        platform_api::SummarizeDirection::UpTo,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("summarize_at ok");

    let session = orch.session();
    let s = session.lock().await;
    let metadata = s
        .history
        .iter()
        .find_map(|message| match message {
            ConversationMessage::System {
                compact_metadata: Some(metadata),
                ..
            } => Some(metadata.clone()),
            _ => None,
        })
        .expect("a compact boundary");
    assert_eq!(
        metadata.user_context.as_deref(),
        Some("keep the parser notes"),
        "the picker text is recorded on the boundary, trimmed"
    );
    assert_eq!(
        metadata.messages_summarized,
        Some(3),
        "three messages preceded the chosen one"
    );
    // `logicalParentUuid` is NOT the oracle's `hn` here: this port sets it from
    // the last on-disk JSONL line uuid (the value has to match a real line), so
    // it stays `None` in a test with no transcript writer. Asserted so nobody
    // "fixes" it by feeding the conversation uuid into the boundary
    // constructor, where `apply_post_compact` would discard it.
    assert_eq!(
        metadata.logical_parent_uuid, None,
        "with no transcript writer there is no JSONL line to point at"
    );
}

/// An ordinary `/compact` must NOT gain the two message-selector fields: a real
/// 2.1.208 transcript never carries them, and a cold resume compares shapes.
#[tokio::test]
async fn an_ordinary_compact_boundary_keeps_the_three_arg_shape() {
    let orch = make_orch();
    seed_history(&orch, 12).await;
    orch.force_compact().await.expect("force_compact ok");

    let session = orch.session();
    let s = session.lock().await;
    let metadata = s
        .history
        .iter()
        .find_map(|message| match message {
            ConversationMessage::System {
                compact_metadata: Some(metadata),
                ..
            } => Some(metadata.clone()),
            _ => None,
        })
        .expect("a compact boundary");
    assert_eq!(metadata.user_context, None);
    assert_eq!(metadata.messages_summarized, None);
}
