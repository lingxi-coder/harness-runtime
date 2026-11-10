use super::*;
use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct LiveOutput(AtomicBool);

#[async_trait]
impl lingxi_core::host::OutputStream for LiveOutput {
    fn wants_forwarded_subagent_messages(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    async fn emit_text(&self, _: &Utf16JsonProjection) {}
    async fn emit_tool_call(
        &self,
        _: &lingxi_core::types::ToolUseId,
        _: &str,
        _: &Utf16JsonProjection,
    ) {
    }
    async fn emit_tool_result(
        &self,
        _: &lingxi_core::types::ToolUseId,
        _: &str,
        _: &Utf16JsonProjection,
        _: &Utf16JsonProjection,
        _: Option<&Utf16JsonProjection>,
    ) {
    }
    async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
}

#[test]
fn forwarded_text_gate_tracks_live_sink_and_expires_with_it() {
    let output = Arc::new(LiveOutput::default());
    let sink: Arc<dyn lingxi_core::host::OutputStream> = output.clone();
    let pool = Arc::new(StateMachinePool::new(
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
        1,
    ));
    let spawner = PoolSubagentSpawner::new(pool).with_forward_subagent_output(&sink);
    assert!(!spawner.should_forward_subagent_text());
    output.0.store(true, Ordering::SeqCst);
    assert!(spawner.should_forward_subagent_text());
    output.0.store(false, Ordering::SeqCst);
    assert!(!spawner.should_forward_subagent_text());
    drop(sink);
    drop(output);
    assert!(!spawner.should_forward_subagent_text());
}

#[derive(Default)]
struct ProgressApi(AtomicUsize);

#[async_trait]
impl SubagentApiClient for ProgressApi {
    async fn stream(
        &self,
        _: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        let content = if first {
            vec![llm_runtime::ContentBlock::ToolCall {
                id: "fixture-read".into(),
                name: "Read".into(),
                input: serde_json::from_str(r#"{"file_path":"fixture.txt"}"#).unwrap(),
            }]
        } else {
            vec![llm_runtime::ContentBlock::Text {
                text: "final assistant text".into(),
                citations: None,
                cache_control: None,
            }]
        };
        let response = llm_runtime::HistoryResponse {
            id: "forwarding-fixture".into(),
            model: "claude-sonnet-5".into(),
            content,
            stop_reason: Some(if first { "tool_use" } else { "end_turn" }.into()),
            stop_details: None,
            usage: llm_runtime::ExecutionUsage::default(),
            cost: None,
            provider_metadata: Default::default(),
            resumable: None,
        };
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

struct ProgressInvoker;

#[async_trait]
impl ToolInvoker for ProgressInvoker {
    async fn invoke(
        &self,
        _: &str,
        _: Utf16JsonProjection,
        _: SubagentInvocationContext,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        Ok("fixture file contents".into())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct ProgressBudget;

#[async_trait]
impl BudgetEnforcerHandle for ProgressBudget {
    async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }

    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

#[tokio::test]
async fn progress_pump_gates_assistant_envelopes_and_keeps_nested_tool_activity() {
    use lingxi_core::host::subagent_spawn::FORWARD_SUBAGENT_MESSAGE_SENTINEL;

    for enabled in [false, true] {
        let output = Arc::new(LiveOutput::default());
        output.0.store(enabled, Ordering::SeqCst);
        let sink: Arc<dyn lingxi_core::host::OutputStream> = output;
        let api = Arc::new(ProgressApi::default());
        let pool = Arc::new(StateMachinePool::new(
            Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
            1,
        ));
        let spawner = PoolSubagentSpawner::new(pool)
            .with_forward_subagent_output(&sink)
            .with_api_client(api.clone())
            .with_default_model("claude-sonnet-5")
            .with_model_resolution_context_provider(Arc::new(
                |model: &str, profile: Option<&str>| {
                    Ok(ModelResolutionContext {
                        route: crate::model_resolution::ModelRouteFacts {
                            model: model.into(),
                            profile: profile.map(str::to_owned),
                            provider: Some(crate::model_resolution::ModelProviderKind::FirstParty),
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                },
            ));
        let request = serde_json::from_value(serde_json::json!({
            "subagent_type":"general-purpose", "prompt":"Read the fixture then finish",
        }))
        .unwrap();
        let (progress, mut received) = tokio::sync::mpsc::channel(32);
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            spawner.spawn_with_progress(
                request,
                SubagentInheritance {
                    tool_invoker: Arc::new(ProgressInvoker),
                    budget: Arc::new(ProgressBudget),
                },
                Some(progress),
            ),
        )
        .await
        .expect("fixture child completes")
        .unwrap();
        assert!(matches!(result, SubagentResult::Completed { .. }));
        assert_eq!(api.0.load(Ordering::SeqCst), 2);
        let mut activity = Vec::new();
        let mut forwarded = Vec::new();
        while let Ok(line) = received.try_recv() {
            if let Ok(source) = Utf16JsonProjection::parse(&line) {
                if let Some(message) = source.get_property(FORWARD_SUBAGENT_MESSAGE_SENTINEL) {
                    forwarded.push(message.to_projection().unwrap());
                    continue;
                }
            }
            activity.push(line);
        }
        assert!(
            activity.iter().any(|line| line == "Read(fixture.txt)"),
            "nested activity was lost with forwarding={enabled}: {activity:?}"
        );
        if enabled {
            assert!(
                forwarded
                    .iter()
                    .any(|message| message.string_at("/content/0/text")
                        == Some("final assistant text")),
                "enabled forwarding must carry the actual assistant payload"
            );
        } else {
            assert!(
                forwarded.is_empty(),
                "disabled forwarding must produce no assistant envelope"
            );
        }
    }
}
