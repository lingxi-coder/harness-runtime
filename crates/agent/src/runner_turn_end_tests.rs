//! Runtime coverage for trusted tool result control, distinct from tool JSON.

use super::*;
use lingxi_core::host::instructions::{
    InstructionContext, InstructionContextProvider, InstructionReadContext, InstructionScope,
};
use lingxi_core::host::tool_invoker::{
    SubagentInvocationContext, ToolInvocationResult, ToolInvokerError, ToolResultTurnEnd,
    ToolResultTurnEndSource,
};
use tracing::instrument::WithSubscriber;

#[derive(Default)]
struct TurnEndTrace {
    sources: Mutex<Vec<String>>,
}

impl tracing::Subscriber for TurnEndTrace {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct Fields {
            event: Option<String>,
            source: Option<String>,
        }
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                match field.name() {
                    "event" => self.event = Some(value.into()),
                    "source" => self.source = Some(value.into()),
                    _ => {}
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.event.as_deref() == Some("tengu_mcp_tool_result_ended_turn") {
            self.sources.lock().unwrap().push(fields.source.unwrap());
        }
    }
}

struct TurnEndInvoker {
    marker: Option<ToolResultTurnEnd>,
    is_error: bool,
    calls: Mutex<Vec<String>>,
    publish_notification: Option<Arc<OwnerNotificationRegistry>>,
}

impl TurnEndInvoker {
    fn new(source: Option<ToolResultTurnEndSource>, is_error: bool) -> Arc<Self> {
        Arc::new(Self {
            marker: source.map(|source| ToolResultTurnEnd { source }),
            is_error,
            calls: Mutex::new(Vec::new()),
            publish_notification: None,
        })
    }
}

#[async_trait]
impl lingxi_core::host::ToolInvoker for TurnEndInvoker {
    async fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: SubagentInvocationContext,
    ) -> Result<serde_json::Value, ToolInvokerError> {
        unreachable!("the runner must use the detailed dispatch boundary")
    }

    async fn invoke_detailed(
        &self,
        name: &str,
        _: serde_json::Value,
        context: SubagentInvocationContext,
    ) -> Result<ToolInvocationResult, ToolInvokerError> {
        assert!(context.tool_use_id.is_some());
        assert!(context.assistant_message_id.is_some());
        self.calls.lock().unwrap().push(name.to_string());
        let finishes = matches!(name, "Finish" | "FinishTool" | "FinishMcp");
        if finishes {
            if let Some(registry) = &self.publish_notification {
                registry.publish();
            }
        }
        Ok(ToolInvocationResult {
            mcp_meta_projection: None,
            model_content_projection: None,
            data_projection: None,
            // These JSON fields alone are intentionally untrusted. The two
            // negative cases below carry the same data without valid control.
            data: serde_json::json!({
                "endsTurn": true,
                "_meta": {"claude/endTurn": true},
                "tool": name,
            }),
            model_content: Some(format!("{name} result")),
            is_error: finishes && self.is_error,
            turn_end: match name {
                "Finish" => self.marker,
                "FinishTool" => Some(ToolResultTurnEnd {
                    source: ToolResultTurnEndSource::Tool,
                }),
                "FinishMcp" => Some(ToolResultTurnEnd {
                    source: ToolResultTurnEndSource::McpMeta,
                }),
                _ => None,
            },
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            context: lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
                serde_json::Value::Array(Vec::new()),
            ),
            context_state: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct TurnEndReadProvider;

#[async_trait]
impl InstructionContextProvider for TurnEndReadProvider {
    async fn load(
        &self,
        _: &std::path::Path,
        _: InstructionScope,
    ) -> Result<InstructionContext, String> {
        Ok(InstructionContext::default())
    }

    async fn after_read(
        &self,
        _: &std::path::Path,
        path: &std::path::Path,
        partial: bool,
        _: &mut InstructionContext,
    ) -> InstructionReadContext {
        assert_eq!(path, std::path::Path::new("/project/pkg/code.rs"));
        assert!(!partial);
        InstructionReadContext {
            legacy_reminders: Vec::new(),
            agents_context: vec![
                "Contents of /project/pkg/AGENTS.md:\n\nKeep this instruction.".into(),
            ],
        }
    }
}

fn turn_end_api(responses: Vec<llm_runtime::HistoryResponse>) -> Arc<ResultStreamMockApiClient> {
    ResultStreamMockApiClient::new(
        responses
            .into_iter()
            .map(|response| {
                llm_runtime::stream_accumulator::response_to_stream_events(response)
                    .into_iter()
                    .map(Ok)
                    .collect()
            })
            .collect(),
    )
}

fn transcript_rows(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn successful_tool_turn_end_finishes_batch_and_persists_read_context_before_completion() {
    for source in [
        ToolResultTurnEndSource::Tool,
        ToolResultTurnEndSource::McpMeta,
    ] {
        let mut response = tool_use_response("Finish", Some("tool_use"));
        response.content.extend([
            llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: "read-after-finish".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "/project/pkg/code.rs"}),
            },
            llm_runtime::ContentBlock::ToolCall {
                input_projection: None,
                id: "write-after-finish".into(),
                name: "Write".into(),
                input: serde_json::json!({}),
            },
        ]);
        let api = turn_end_api(vec![
            response,
            text_response("must not query again", Some("end_turn")),
        ]);
        let invoker = TurnEndInvoker::new(Some(source), false);
        let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
        ctx.allowed_tools = vec!["Finish".into(), "Read".into(), "Write".into()];
        ctx.prompt_messages = vec![ConversationMessage::user(
            MessageId::new(),
            "finish the task".into(),
        )];
        ctx.instruction_provider = Some(Arc::new(TurnEndReadProvider));
        let dir = tempfile::tempdir().unwrap();
        ctx.transcript_subdir = dir.path().to_path_buf();
        ctx.transcript_fs = Some(Arc::new(platform_posix::PosixFileSystem::new(
            dir.path().to_path_buf(),
        )));
        let path = dir.path().join(format!("agent-{}.jsonl", ctx.agent_id));
        let (event_tx, event_rx) = mpsc::channel(8);
        let (out_tx, mut out_rx) = mpsc::channel(32);
        let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
        let mut events = Vec::new();
        while let Some(event) = out_rx.recv().await {
            let completed = matches!(event, SubagentEvent::Completed { .. });
            events.push(event);
            if completed {
                break;
            }
        }
        // Read immediately on completion, before joining the runner: a later
        // best-effort flush cannot make this durability assertion pass.
        let rows = transcript_rows(&path);
        assert_eq!(api.call_count(), 1, "{source:?} must end the query");
        assert_eq!(*invoker.calls.lock().unwrap(), ["Finish", "Read", "Write"]);
        let tool_results = rows
            .iter()
            .filter_map(|row| row["message"]["content"].as_array())
            .flatten()
            .filter(|block| block["type"] == "tool_result")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(tool_results.len(), 3);
        assert_eq!(tool_results[0]["content"], "Finish result");
        assert_eq!(tool_results[1]["tool_use_id"], "read-after-finish");
        assert_eq!(tool_results[1]["content"], "Read result");
        assert_eq!(tool_results[2]["tool_use_id"], "write-after-finish");
        assert_eq!(tool_results[2]["content"], "Write result");
        assert!(tool_results.iter().all(|block| block["is_error"] == false));
        let reminder = rows
            .iter()
            .find(|row| row["attachment"]["type"] == "hook_additional_context")
            .expect("the later Read attachment is durable");
        assert_eq!(
            reminder["message"]["content"][0]["text"],
            "<system-reminder>\ntool.call hook additional context: Contents of /project/pkg/AGENTS.md:\n\nKeep this instruction.\n</system-reminder>"
        );
        assert_eq!(rows.last().unwrap()["status"], "completed");
        assert!(!rows
            .iter()
            .any(|row| row["message"]["subtype"] == "instruction_context"));
        assert_eq!(one_completed(&events)["stop_reason"], "tool_use");
        assert!(events.iter().any(|event| matches!(
            event,
            SubagentEvent::Completed {
                total_tool_use_count: 3,
                ..
            }
        )));
        let request_histories = api.histories.lock().unwrap();
        assert_eq!(request_histories.len(), 1);
        assert!(
            matches!(&request_histories[0][0], ConversationMessage::User { content, .. }
            if content == &[ContentBlock::Text { text: "finish the task".into(), citations: None }])
        );
        drop(request_histories);
        drop(event_tx);
        runner.await.unwrap();
    }
}

#[tokio::test]
async fn failed_tool_marker_and_untrusted_json_do_not_end_the_query() {
    for (source, is_error) in [
        (Some(ToolResultTurnEndSource::Tool), true),
        (Some(ToolResultTurnEndSource::McpMeta), true),
        (None, false),
    ] {
        let api = turn_end_api(vec![
            tool_use_response("Finish", Some("tool_use")),
            text_response("recovered", Some("end_turn")),
        ]);
        let invoker = TurnEndInvoker::new(source, is_error);
        let ctx = loop_ctx(api.clone(), Some(invoker), 4);
        let (_event_tx, event_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(32);
        run_subagent(ctx, event_rx, out_tx).await;
        let events = drain(out_rx).await;
        assert_eq!(api.call_count(), 2, "{source:?}, error={is_error}");
        assert_eq!(one_completed(&events)["text"], "recovered");
        assert!(api.histories.lock().unwrap()[1].iter().any(|message| matches!(
            message, ConversationMessage::User { content, .. } if content.iter().any(|block| matches!(
                block, ContentBlock::ToolResult { content, is_error: actual_error, .. }
                    if content == "Finish result" && actual_error.unwrap_or(false) == is_error
            ))
        )));
    }
}

#[tokio::test]
async fn tool_turn_end_uses_last_accepted_batch_source_once() {
    // Keep two registered dispatchers alive: tracing's single-dispatch fast
    // path otherwise registers a shared callsite against whichever test
    // thread first reaches it, even when that thread has no scoped subscriber.
    let _untraced_dispatch = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    for (first, last, expected) in [
        ("FinishTool", "FinishMcp", "mcp_meta"),
        ("FinishMcp", "FinishTool", "tool"),
    ] {
        let mut response = tool_use_response(first, Some("tool_use"));
        response.content.push(llm_runtime::ContentBlock::ToolCall {
            input_projection: None,
            id: "last-marker".into(),
            name: last.into(),
            input: serde_json::json!({}),
        });
        let api = turn_end_api(vec![response]);
        let invoker = TurnEndInvoker::new(None, false);
        let ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
        let trace = Arc::new(TurnEndTrace::default());
        let dispatcher = tracing::Dispatch::new(trace.clone());
        // Register the production callsite from an untraced real runner first,
        // reproducing the other-thread registration that full-suite runs hit.
        let untraced_api = turn_end_api(vec![tool_use_response("FinishMcp", Some("tool_use"))]);
        let untraced_ctx = loop_ctx(
            untraced_api.clone(),
            Some(TurnEndInvoker::new(None, false)),
            4,
        );
        let (_untraced_event_tx, untraced_event_rx) = mpsc::channel(8);
        let (untraced_out_tx, untraced_out_rx) = mpsc::channel(32);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_subagent(untraced_ctx, untraced_event_rx, untraced_out_tx),
        )
        .await
        .expect("untraced probe runner completes");
        one_completed(&drain(untraced_out_rx).await);
        assert_eq!(untraced_api.call_count(), 1);
        assert!(trace.sources.lock().unwrap().is_empty());
        let (_event_tx, event_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(32);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_subagent(ctx, event_rx, out_tx).with_subscriber(dispatcher),
        )
        .await
        .expect("traced runner and its synchronous completion event settle");
        let events = drain(out_rx).await;
        assert_eq!(api.call_count(), 1);
        one_completed(&events);
        assert_eq!(*invoker.calls.lock().unwrap(), [first, last]);
        assert_eq!(*trace.sources.lock().unwrap(), [expected]);
    }
}

#[tokio::test]
async fn tool_turn_end_wins_over_refusal_fallback_without_requery() {
    let api = turn_end_api(vec![
        tool_use_response("Finish", Some("refusal")),
        text_response("must not use fallback", Some("end_turn")),
    ]);
    let mut ctx = loop_ctx(
        api.clone(),
        Some(TurnEndInvoker::new(
            Some(ToolResultTurnEndSource::Tool),
            false,
        )),
        4,
    );
    ctx.refusal_fallback_chain = vec!["fallback-model".into()];
    let (_event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, out_rx) = mpsc::channel(32);
    run_subagent(ctx, event_rx, out_tx).await;
    let events = drain(out_rx).await;
    assert_eq!(api.call_count(), 1);
    assert_eq!(one_completed(&events)["stop_reason"], "refusal");
    assert!(!events.iter().any(
        |event| matches!(event, SubagentEvent::Message { message, .. }
        if message["subtype"] == "model_refusal_fallback")
    ));
}

#[tokio::test]
async fn tool_turn_end_stops_schema_nudges_but_preserves_final_schema_failure() {
    for stop_reason in ["tool_use", "max_tokens"] {
        let api = turn_end_api(vec![
            tool_use_response("Finish", Some(stop_reason)),
            text_response("must not query for schema", Some("end_turn")),
        ]);
        let mut ctx = loop_ctx(
            api.clone(),
            Some(TurnEndInvoker::new(
                Some(ToolResultTurnEndSource::Tool),
                false,
            )),
            4,
        );
        ctx.schema = Some(
            r#"{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}"#.into(),
        );
        ctx.structured_output_parse_retries = 1;
        let (_event_tx, event_rx) = mpsc::channel(8);
        let (out_tx, out_rx) = mpsc::channel(32);
        run_subagent(ctx, event_rx, out_tx).await;
        let events = drain(out_rx).await;
        assert_eq!(
            api.call_count(),
            1,
            "terminal marker wins over {stop_reason}"
        );
        assert!(events.iter().any(|event| matches!(event, SubagentEvent::Failed { error, .. }
            if error == "agent({schema}): subagent completed without calling StructuredOutput (after in-conversation nudge)")));
        assert!(!events
            .iter()
            .any(|event| matches!(event, SubagentEvent::Completed { .. })));
        assert!(events.iter().any(|event| matches!(event, SubagentEvent::Message { message, .. }
            if message["content"].as_array().is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result" && block["content"] == "Finish result")))));
        assert!(!events.iter().any(|event| matches!(event, SubagentEvent::Message { message, .. }
            if message.to_string().contains("You did not call StructuredOutput") || message.to_string().contains("Continue the unfinished work"))));
    }
}

#[tokio::test]
async fn persistent_tool_turn_end_resets_on_a_later_real_message() {
    let api = turn_end_api(vec![
        tool_use_response("Finish", Some("tool_use")),
        tool_use_response("Read", Some("tool_use")),
        text_response("after wake", Some("end_turn")),
    ]);
    let invoker = TurnEndInvoker::new(Some(ToolResultTurnEndSource::Tool), false);
    let mut ctx = loop_ctx(api.clone(), Some(invoker.clone()), 4);
    ctx.persistent = true;
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(32);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !matches!(
            out_rx.recv().await.expect("first turn-set completes"),
            SubagentEvent::Completed { .. }
        ) {}
        assert_eq!(api.call_count(), 1);
        assert!(!runner.is_finished());
        event_tx
            .send(lingxi_core::Event::UserMessage {
                message_id: MessageId::new(),
                request_id: RequestId::new(),
                content: "continue with a real task".into(),
            })
            .await
            .unwrap();
        loop {
            if let SubagentEvent::Completed { result, .. } =
                out_rx.recv().await.expect("next turn-set completes")
            {
                assert_eq!(result["text"], "after wake");
                break;
            }
        }
    })
    .await
    .expect("both turn-sets complete");
    assert_eq!(
        api.call_count(),
        3,
        "the previous terminal marker must reset"
    );
    assert_eq!(*invoker.calls.lock().unwrap(), ["Finish", "Read"]);
    assert!(api.histories.lock().unwrap()[2]
        .iter()
        .any(|message| matches!(message,
        ConversationMessage::User { content, .. } if content.iter().any(|block| matches!(block,
            ContentBlock::Text { text, .. } if text == "continue with a real task")))));
    drop(event_tx);
    runner.await.unwrap();
}

#[tokio::test]
async fn tool_turn_end_parks_owned_work_before_pending_notification_can_wake_it() {
    let api = turn_end_api(vec![
        tool_use_response("Finish", Some("tool_use")),
        text_response("after background work", Some("end_turn")),
    ]);
    let mut ctx = loop_ctx(api.clone(), None, 4);
    let registry = Arc::new(OwnerNotificationRegistry {
        park_foreground: true,
        rest_acknowledged: AtomicBool::new(false),
        wake_checked: tokio::sync::Notify::new(),
        drains: AtomicUsize::new(0),
        parked_fold: tokio::sync::Notify::new(),
        owner: ctx.agent_id,
        pending: Mutex::new(Vec::new()),
        agent_fact_updates: Mutex::new(Vec::new()),
        records: Mutex::new(Vec::new()),
        revision: tokio::sync::watch::channel(0).0,
    });
    ctx.tool_invoker = Some(Arc::new(TurnEndInvoker {
        marker: Some(ToolResultTurnEnd {
            source: ToolResultTurnEndSource::Tool,
        }),
        is_error: false,
        calls: Mutex::new(Vec::new()),
        publish_notification: Some(registry.clone()),
    }));
    ctx.task_registry = Some(registry.clone());
    let (event_tx, event_rx) = mpsc::channel(8);
    let (out_tx, mut out_rx) = mpsc::channel(32);
    let runner = tokio::spawn(run_subagent(ctx, event_rx, out_tx));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match out_rx.recv().await.expect("owned-work runner remains live") {
                SubagentEvent::Message { message, .. } if message["subtype"] == "agent_idle" => {
                    break;
                }
                SubagentEvent::Completed { .. } => {
                    panic!("owned work must preserve the foreground pump")
                }
                _ => {}
            }
        }
        assert_eq!(
            api.call_count(),
            1,
            "pending work must not override terminal control"
        );
        assert_eq!(registry.pending.lock().unwrap().len(), 1);
        assert!(!runner.is_finished());
        registry.rest_acknowledged.store(true, Ordering::SeqCst);
        registry.revision.send_modify(|revision| *revision += 1);
        loop {
            if let SubagentEvent::Message { message, .. } = out_rx
                .recv()
                .await
                .expect("notification resumes owned work")
            {
                if message["subtype"] == "agent_idle" {
                    break;
                }
            }
        }
    })
    .await
    .expect("terminal park and later background notification wake complete");
    assert_eq!(api.call_count(), 2);
    assert!(api.histories.lock().unwrap()[1]
        .iter()
        .any(|message| matches!(message,
        ConversationMessage::User { content, .. } if content.iter().any(|block| matches!(block,
            ContentBlock::Text { text, .. } if text.contains("child finished"))))));
    drop(event_tx);
    runner.await.unwrap();
}
