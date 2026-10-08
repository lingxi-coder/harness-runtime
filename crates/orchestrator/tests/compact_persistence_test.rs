//! Claude Code 2.1.286 post-compact JSONL persistence and cold-resume parity.
//!
//! Both full automatic compaction and tail-preserving manual compaction persist
//! a chain-reset boundary and a typed summary. Only the manual path records
//! preservedMessages for the loader to splice back into the resumed history.
//! Announced session-context/date and total-token rows advance the parent chain.
//! These tests drive the actual compaction and writer, then compare every
//! loaded history entry with the live session and verify the complete chain.

use llm_runtime::ContentBlock as LlmContentBlock;

use compaction::CompactionOrchestrator;
use lingxi_core::host::{FileSystem, OrchestratorHandle};
use lingxi_core::types::ConversationMessage;
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{state_from_messages, ConversationOrchestrator, OrchestratorConfig};
use platform_posix::fs::PosixFileSystem;
use serde_json::Value;
use session::jsonl::loader::build_conversation_chain;
use session::jsonl::reader::JsonlReader;
use session::jsonl::writer::JsonlWriter;
use session::jsonl::JsonlMessage;
use std::sync::Arc;
use tempfile::tempdir;

struct SummaryClient;

#[async_trait::async_trait]
impl sidequery::SideQueryClient for SummaryClient {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        Ok(sidequery::SideQueryResponse {
            text: Some("<summary>Earlier turns summarized.</summary>".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

/// The (kind, text) shape used to compare hot vs cold history — message ids
/// differ across a persist/reload cycle (assistant turns are persisted
/// per-block with fresh line uuids), so identity is content-based.
fn shape(history: &[ConversationMessage]) -> Vec<(&'static str, String)> {
    history
        .iter()
        .map(|m| {
            let kind = match m {
                ConversationMessage::User { .. } => "user",
                ConversationMessage::Assistant { .. } => "assistant",
                ConversationMessage::System { .. } => "system",
            };
            (kind, m.text_content())
        })
        .collect()
}

/// Inner `message.content` text of a JSONL line — accepts both the plain
/// string form and the content-block array the persist path writes.
fn inner_text(line: &JsonlMessage) -> String {
    match line.message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Actual native Or/Br fixture for empty eager files, followed by HZo's stock
/// reanchored total-token attachment. Only the machine's local date is replaced;
/// types, field presence and rendered bytes follow executed native helpers.
fn assert_empty_context_and_budget_announcements<'a>(
    rows: &'a [JsonlMessage],
    hot_history: &[ConversationMessage],
    parent_uuid: &str,
) -> &'a JsonlMessage {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/instruction_announcements_2_1_286.json"
    ))
    .expect("executed native context fixture");
    let case = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "successful-empty-initial")
        .expect("native empty-file announcement case");
    let mut expected = case["expected"].as_array().unwrap().clone();
    assert_eq!(expected.len(), 2);
    // 2.1.286 HZo reanchors a regular user prompt before computing remaining
    // tokens. Actual wvt/HZo/ol execution with the stock settings yields this
    // exact payload and body, even when a previous context rolled over.
    expected.push(serde_json::json!({
        "attachment": {
            "type": "total_tokens_reminder",
            "text": "<total_tokens>15000000 tokens left</total_tokens>"
        },
        "rendered": [
            "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>"
        ]
    }));
    assert_eq!(
        rows.len(),
        expected.len(),
        "exact native announcement count"
    );
    let today = orchestrator::prompt::env_meta::current_date_string();
    let mut parent = parent_uuid.to_owned();
    for (row, native) in rows.iter().zip(&expected) {
        let mut payload = native["attachment"].clone();
        let native_date = payload
            .get("date")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if native_date.is_some() {
            payload["date"] = Value::String(today.clone());
        }
        assert_eq!(row.message_type, "attachment");
        assert_eq!(
            row.message,
            Value::Null,
            "native attachment has no inner message"
        );
        assert_eq!(row.extra.get("attachment"), Some(&payload));
        assert!(!row.is_sidechain);
        assert_eq!(row.logical_parent_uuid, None);
        assert_eq!(row.parent_uuid.as_deref(), Some(parent.as_str()));
        let projection = hot_history
            .iter()
            .find(|message| message.id().as_uuid().to_string() == row.uuid)
            .expect("every raw announcement retains its hot projection identity");
        let rendered = native["rendered"].as_array().unwrap();
        if rendered.is_empty() {
            assert!(matches!(projection, ConversationMessage::System {
                content, subtype: Some(subtype), ..
            } if content.is_empty() && subtype == "model_reminder_attachment"));
        } else {
            assert_eq!(rendered.len(), 1);
            assert!(projection.is_meta());
            let bytes = rendered[0].as_str().unwrap();
            let expected_body = native_date
                .as_deref()
                .map_or_else(|| bytes.to_owned(), |date| bytes.replace(date, &today));
            assert_eq!(
                projection.text_content().as_bytes(),
                expected_body.as_bytes()
            );
        }
        parent = row.uuid.clone();
    }
    rows.last().unwrap()
}

#[test]
fn cold_resume_reconstructs_full_auto_compact_state() {
    assert_cold_resume_reconstructs_post_compact_state(false);
}

#[test]
fn cold_resume_reconstructs_manual_compact_state_with_preserved_tail() {
    assert_cold_resume_reconstructs_post_compact_state(true);
}

fn assert_cold_resume_reconstructs_post_compact_state(manual: bool) {
    std::thread::Builder::new()
        .name("compact-persistence".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("compact persistence runtime")
                .block_on(cold_resume_reconstructs_post_compact_state_inner(manual));
        })
        .expect("spawn compact persistence test thread")
        .join()
        .expect("compact persistence test thread");
}

async fn cold_resume_reconstructs_post_compact_state_inner(manual: bool) {
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    // The fourth prompt trips automatic compaction. The manual case uses a
    // high threshold, compacts explicitly after that reply, then continues.
    let responses: Vec<_> = [
        "ok one",
        "ok two",
        "ok three",
        "final reply",
        "continued reply",
    ]
    .iter()
    .map(|t| {
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: (*t).to_string(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        )
    })
    .collect();
    let api = Arc::new(MockApiClient::new(responses));
    let output = Arc::new(MockOutputStream::new());
    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(Arc::new(SummaryClient), "test-compact-model".into()),
    );
    let compactor = CompactionOrchestrator::with_autocompactor(
        compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
        if manual { u64::MAX } else { 200 },
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer)
    .with_cache_safe_slot(slot)
    .with_compaction(Arc::new(compactor));

    // Three small persisted turns...
    orch.run_turn("small one").await.expect("turn 1");
    orch.run_turn("small two").await.expect("turn 2");
    orch.run_turn("small three").await.expect("turn 3");
    // Automatic compaction persists the boundary and summary before the call;
    // manual compaction persists them between turns and retains the last round.
    let big_prompt = format!("analyze this: {}", "x".repeat(8000));
    orch.run_turn(&big_prompt).await.expect("turn 4");
    if manual {
        orch.force_compact().await.expect("manual compact");
        orch.run_turn("continue after compact")
            .await
            .expect("turn 5");
    }

    // Hot post-compact state.
    let hot_history = {
        let session = orch.session();
        let s = session.lock().await;
        s.history.clone()
    };
    let boundary_pos = hot_history
        .iter()
        .position(compaction::is_compact_boundary)
        .expect("hot history carries the compact boundary marker");
    assert_eq!(
        boundary_pos, 0,
        "boundary marker leads the compacted history"
    );
    let hot_boundary_metadata = match &hot_history[boundary_pos] {
        ConversationMessage::System {
            subtype: Some(subtype),
            compact_metadata: Some(metadata),
            ..
        } => {
            assert_eq!(subtype, "compact_boundary");
            metadata.clone()
        }
        other => panic!("expected typed compact boundary, got {other:?}"),
    };
    assert!(
        hot_history
            .iter()
            .any(ConversationMessage::is_compact_summary),
        "hot history carries the typed compact-summary flag"
    );

    // ---- On-disk shape (Claude Code 2.1.286) ---------------------------------- //
    let reader = JsonlReader::new(session_path.clone(), fs.clone());
    let lines: Vec<JsonlMessage> = reader.read_all().await.expect("read_all");

    let boundary_idx = lines
        .iter()
        .position(|l| {
            l.message_type == "system"
                && l.extra.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        })
        .expect("a compact_boundary system line must be persisted");
    let boundary = &lines[boundary_idx];
    assert!(boundary_idx > 0, "boundary follows the pre-compact lines");
    // Chain reset: parentUuid null; the real parent (the last pre-compact
    // on-disk line) rides in logicalParentUuid.
    assert_eq!(boundary.parent_uuid, None, "boundary line resets the chain");
    assert_eq!(
        boundary.logical_parent_uuid.as_deref(),
        Some(lines[boundary_idx - 1].uuid.as_str()),
        "logicalParentUuid = the last pre-compact line's uuid"
    );
    assert_eq!(
        boundary.extra.get("content").and_then(Value::as_str),
        Some("Conversation compacted")
    );
    assert_eq!(
        boundary.extra.get("level").and_then(Value::as_str),
        Some("info")
    );
    let cm = boundary
        .extra
        .get("compactMetadata")
        .expect("compactMetadata persisted");
    let mut expected_cm =
        serde_json::to_value(&hot_boundary_metadata).expect("hot compact metadata serializes");
    expected_cm
        .as_object_mut()
        .expect("compact metadata object")
        .remove("logicalParentUuid");
    assert_eq!(
        cm, &expected_cm,
        "hot typed metadata and persisted compactMetadata stay identical"
    );
    assert_eq!(
        cm.get("trigger").and_then(Value::as_str),
        Some(if manual { "manual" } else { "auto" })
    );
    assert!(
        cm.get("postTokens").and_then(Value::as_u64).is_some(),
        "successful compaction records the rebuilt context size"
    );
    assert!(
        cm.get("durationMs").and_then(Value::as_u64).is_some(),
        "successful compaction records its wall duration"
    );
    assert!(
        cm.get("cumulativeDroppedTokens")
            .and_then(Value::as_u64)
            .is_some(),
        "successful compaction records cumulative dropped tokens"
    );

    // The summary user line chains off the boundary and carries the flags.
    let summary = &lines[boundary_idx + 1];
    assert_eq!(summary.message_type, "user");
    assert_eq!(
        summary.parent_uuid.as_deref(),
        Some(boundary.uuid.as_str()),
        "summary chains off the boundary"
    );
    assert_eq!(
        summary.extra.get("isCompactSummary"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        summary.extra.get("isVisibleInTranscriptOnly"),
        Some(&Value::Bool(true))
    );

    // Native Br emits fresh session_context/date when the compacted model
    // history has no announcement baseline. Native insertMessageChain updates
    // its leaf for attachments too (`$ge` excludes only progress).
    // The stock total-token producer follows these rows and also advances the
    // chain, so the assistant must parent off that final attachment.
    // Full auto replaces all pre-compact history. Manual compact preserves the
    // final API-round group, which here is the last assistant reply.
    let expected_post_types = if manual {
        vec![
            "system",
            "user",
            "user",
            "attachment",
            "attachment",
            "attachment",
            "assistant",
        ]
    } else {
        vec![
            "system",
            "user",
            "attachment",
            "attachment",
            "attachment",
            "assistant",
        ]
    };
    assert_eq!(
        lines[boundary_idx..]
            .iter()
            .map(|line| line.message_type.as_str())
            .collect::<Vec<_>>(),
        expected_post_types,
        "exact physical post-compact sequence; no duplicate or skipped announcements"
    );
    if manual {
        let pm = cm
            .get("preservedMessages")
            .expect("manual tail metadata persisted");
        assert_eq!(
            pm.get("anchorUuid").and_then(Value::as_str),
            Some(summary.uuid.as_str()),
            "preserved tail anchors on the summary"
        );
        let tail = &lines[boundary_idx - 1];
        assert_eq!(tail.message_type, "assistant");
        assert_eq!(inner_text(tail), "final reply");
        assert_eq!(
            pm.get("uuids"),
            Some(&serde_json::json!([tail.uuid])),
            "manual compaction preserves exactly the last API-round group"
        );
        let continuation = &lines[boundary_idx + 2];
        assert_eq!(continuation.message_type, "user");
        assert_eq!(inner_text(continuation), "continue after compact");
        assert_eq!(
            continuation.parent_uuid.as_deref(),
            Some(tail.uuid.as_str()),
            "continued history chains off the original persisted tail"
        );
        let budget = assert_empty_context_and_budget_announcements(
            &lines[boundary_idx + 3..boundary_idx + 6],
            &hot_history,
            &continuation.uuid,
        );
        let reply = &lines[boundary_idx + 6];
        assert_eq!(reply.message_type, "assistant");
        assert_eq!(inner_text(reply), "continued reply");
        assert_eq!(reply.parent_uuid.as_deref(), Some(budget.uuid.as_str()));
        assert_eq!(hot_history.len(), 8);
        assert!(matches!(
            &hot_history[2],
            ConversationMessage::Assistant { .. }
        ));
        assert_eq!(hot_history[2].text_content(), "final reply");
        assert_eq!(hot_history[3].text_content(), "continue after compact");
        assert!(matches!(
            &hot_history[7],
            ConversationMessage::Assistant { .. }
        ));
        assert_eq!(hot_history[7].text_content(), "continued reply");
    } else {
        assert!(
            cm.get("preservedMessages").is_none(),
            "full automatic compaction does not preserve a verbatim tail"
        );
        let budget = assert_empty_context_and_budget_announcements(
            &lines[boundary_idx + 2..boundary_idx + 5],
            &hot_history,
            &summary.uuid,
        );
        let reply = &lines[boundary_idx + 5];
        assert_eq!(reply.message_type, "assistant");
        assert_eq!(inner_text(reply), "final reply");
        assert_eq!(
            reply.parent_uuid.as_deref(),
            Some(budget.uuid.as_str()),
            "full auto follows summary, session_context, date, budget and reply in order"
        );
        assert_eq!(hot_history.len(), 6);
        assert!(matches!(
            &hot_history[5],
            ConversationMessage::Assistant { .. }
        ));
        assert_eq!(hot_history[5].text_content(), "final reply");
    }
    assert!(
        lines[..boundary_idx]
            .iter()
            .any(|line| { line.message_type == "user" && inner_text(line) == big_prompt }),
        "the full original prompt remains in the persisted pre-compact transcript"
    );
    assert!(
        !hot_history
            .iter()
            .any(|message| message.text_content() == big_prompt),
        "a summarized prompt is not retained as a standalone context message"
    );

    // ---- Cold reload == hot state ---------------------------------------- //
    let loaded = reader.read_routed().await.expect("read_routed");
    let (chain, _tip_sid) = build_conversation_chain(&loaded, "cold-load");
    let session_uuid = uuid::Uuid::new_v4();
    let cold_state = state_from_messages(session_uuid, &chain);

    let post_context_rows = &lines
        [boundary_idx + if manual { 3 } else { 2 }..boundary_idx + if manual { 6 } else { 5 }];
    let cold_context_rows = chain
        .iter()
        .filter(|row| row.message_type == "attachment")
        .collect::<Vec<_>>();
    assert_eq!(
        cold_context_rows.len(),
        3,
        "cold model chain retains exactly context, date and budget announcements"
    );
    for (cold, persisted) in cold_context_rows.iter().zip(post_context_rows) {
        assert_eq!(cold.uuid, persisted.uuid);
        assert_eq!(cold.parent_uuid, persisted.parent_uuid);
        assert_eq!(
            serde_json::to_vec(&cold.extra["attachment"]).unwrap(),
            serde_json::to_vec(&persisted.extra["attachment"]).unwrap(),
            "cold replay retains every typed attachment payload byte"
        );
    }

    // The boundary, summary flags, re-spliced preserved tail, and
    // post-compact reply all use the same typed representation after a cold
    // resume. No summarized pre-compact message may re-enter.
    assert_eq!(
        shape(&cold_state.history),
        shape(&hot_history),
        "cold-resume history must equal the hot post-compact history"
    );
    assert!(compaction::is_compact_boundary(&cold_state.history[0]));
    assert_eq!(
        cold_state.history[0], hot_history[0],
        "cold-resume boundary retains the hot typed metadata"
    );
    assert!(cold_state
        .history
        .iter()
        .any(ConversationMessage::is_compact_summary));
    for (_, text) in shape(&cold_state.history) {
        assert!(
            !text.contains("small one") || text.contains("Summary"),
            "summarized pre-compact prompt must not replay verbatim: {text}"
        );
    }
    // The summarized prefix's user prompts are gone from cold history as
    // standalone messages.
    assert!(
        !cold_state
            .history
            .iter()
            .any(|m| matches!(m, ConversationMessage::User { .. })
                && m.text_content() == "small one"),
        "pre-compact user prompt replayed into cold history"
    );
}

/// RV6 (parity 2.1.208) — the persisted compact boundary's `compactMetadata`
/// carries `preCompactDiscoveredTools` (the deferred tools loaded via ToolSearch
/// before the compaction, `Age()`/`[...B].sort()`) but NOT `messagesSummarized`:
/// CC's full auto/manual compaction path builds the boundary with the 3-arg
/// `U6r(trigger, preTokens, lastUuid)`, leaving `messagesSummarized` undefined
/// (dropped by `JSON.stringify`). Only the unported message-selector
/// (`up_to`/`from`) path sets `messagesSummarized:f.length`.
#[test]
fn compact_boundary_carries_discovered_tools_and_omits_messages_summarized() {
    std::thread::Builder::new()
        .name("compact-boundary-persistence".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("compact boundary runtime")
                .block_on(
                    compact_boundary_carries_discovered_tools_and_omits_messages_summarized_inner(),
                );
        })
        .expect("spawn compact boundary test thread")
        .join()
        .expect("compact boundary test thread");
}

async fn compact_boundary_carries_discovered_tools_and_omits_messages_summarized_inner() {
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs.clone()));

    let responses: Vec<_> = ["ok one", "ok two", "ok three", "final reply"]
        .iter()
        .map(|t| {
            mock_message_response(
                vec![LlmContentBlock::Text {
                    text: (*t).to_string(),
                    cache_control: None, citations: None,
                }],
                Some("end_turn"),
            )
        })
        .collect();
    let api = Arc::new(MockApiClient::new(responses));
    let output = Arc::new(MockOutputStream::new());

    // Simulate two deferred tools the model loaded via ToolSearch BEFORE the
    // compaction: they seed the deferral loaded-set, the carry source for
    // `preCompactDiscoveredTools`. `mark_loaded` populates the set regardless of
    // the (default-off) Tool Search mode, exercising the carry in isolation.
    let registry = tool_api::registry::ToolRegistry::new();
    registry
        .deferral()
        .mark_loaded(["WebFetch".to_string(), "Task".to_string()]);
    let registry = Arc::new(registry);

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        registry,
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer)
    .with_compaction(Arc::new(CompactionOrchestrator::new(200)));

    orch.run_turn("small one").await.expect("turn 1");
    orch.run_turn("small two").await.expect("turn 2");
    orch.run_turn("small three").await.expect("turn 3");
    let big_prompt = format!("analyze this: {}", "x".repeat(8000));
    orch.run_turn(&big_prompt).await.expect("turn 4");

    let reader = JsonlReader::new(session_path.clone(), fs.clone());
    let lines: Vec<JsonlMessage> = reader.read_all().await.expect("read_all");
    let boundary = lines
        .iter()
        .find(|l| {
            l.message_type == "system"
                && l.extra.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        })
        .expect("a compact_boundary system line must be persisted");
    let cm = boundary
        .extra
        .get("compactMetadata")
        .expect("compactMetadata persisted");

    // preCompactDiscoveredTools carries the loaded set, sorted + deduped.
    let discovered: Vec<&str> = cm
        .get("preCompactDiscoveredTools")
        .and_then(Value::as_array)
        .expect("preCompactDiscoveredTools present")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        discovered,
        vec!["Task", "WebFetch"],
        "boundary carries the ToolSearch-loaded set, sorted"
    );

    // messagesSummarized is NOT emitted on the full compaction path: CC's 3-arg
    // boundary constructor leaves it undefined (dropped by `JSON.stringify`), so
    // a real 2.1.208 transcript never carries it here. Only the unported
    // message-selector (up_to/from) path sets it.
    assert!(
        cm.get("messagesSummarized").is_none(),
        "full compaction boundary must not carry messagesSummarized, got {:?}",
        cm.get("messagesSummarized")
    );

    // The scan half of the carry round-trips: the resume loader recovers the
    // same discovered set from the persisted boundary.
    let recovered = session::jsonl::pre_compact_discovered_tools(&lines);
    assert_eq!(recovered, vec!["Task".to_string(), "WebFetch".to_string()]);
}

/// `isCompactSummary` user lines replay into model history while retaining both
/// independent JSONL visibility flags in the session side tables.
#[test]
fn compact_summary_line_replays_as_user_history() {
    let mut extra = serde_json::Map::new();
    extra.insert("isVisibleInTranscriptOnly".to_string(), Value::Bool(true));
    extra.insert("isCompactSummary".to_string(), Value::Bool(true));
    let summary = JsonlMessage { json_projection: None,
        message_type: "user".to_string(),
        uuid: "9a1b2c3d-4e5f-6789-abcd-ef0123456789".to_string(),
        parent_uuid: None,
        session_id: "11111111-2222-3333-4444-555555555555".to_string(),
        timestamp: "2026-07-13T10:00:00.000Z".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.6.0".to_string(),
        message: serde_json::json!({"role":"user","content":"Summary:\nS"}),
        is_sidechain: false,
        user_type: Some("external".to_string()),
        git_branch: None,
        entrypoint: None,
        slug: None,
        prompt_id: None,
        logical_parent_uuid: None,
        extra,
    };
    let state = state_from_messages(uuid::Uuid::new_v4(), &[summary]);
    assert_eq!(state.history.len(), 1, "summary line replays into history");
    match &state.history[0] {
        ConversationMessage::User {
            is_meta,
            is_compact_summary,
            is_visible_in_transcript_only,
            ..
        } => {
            assert!(!is_meta, "summary replays as a NORMAL user message");
            assert!(*is_compact_summary);
            assert!(*is_visible_in_transcript_only);
        }
        other => panic!("expected a user message, got {other:?}"),
    }
    assert_eq!(state.history[0].text_content(), "Summary:\nS");
    let id = state.history[0].id();
    assert!(state.transcript_only_messages.contains(&id));
    assert!(state.compact_summary_messages.contains(&id));
}
