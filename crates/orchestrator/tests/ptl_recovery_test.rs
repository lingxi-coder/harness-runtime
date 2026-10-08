//! Provider overflow recovery preserves original history until a real summary succeeds.
use async_trait::async_trait;
use compaction::CompactionOrchestrator;
use lingxi_core::host::OutputEvent;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use llm_runtime::{ContentBlock as LlmContentBlock, HistoryResponse, LlmError};
use orchestrator::test_support::{
    mock_message_response, noop_hook_executor, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorApiClient, OrchestratorConfig};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A mock API client whose queue is `Result<HistoryResponse, LlmError>` so a
/// test can script `LlmError::ContextOverflow` responses. Captures per-call
/// message counts so the test can assert head-truncation shrank the prompt.
struct PtlMockApi {
    queue: Mutex<VecDeque<Result<HistoryResponse, LlmError>>>,
    captured_lens: Mutex<Vec<usize>>,
}

impl PtlMockApi {
    fn new(script: Vec<Result<HistoryResponse, LlmError>>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::from(script)),
            captured_lens: Mutex::new(Vec::new()),
        }
    }
    async fn call_lens(&self) -> Vec<usize> {
        self.captured_lens.lock().await.clone()
    }
}

#[async_trait]
impl OrchestratorApiClient for PtlMockApi {
    async fn messages_create(
        &self,
        request: orchestrator::OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        let msgs = match request {
            orchestrator::OrchestratorApiRequest::Main(request) => request.messages,
            orchestrator::OrchestratorApiRequest::HookPrompt(request) => request.messages,
        };

        self.captured_lens.lock().await.push(msgs.len());
        let mut q = self.queue.lock().await;
        q.pop_front().unwrap_or_else(|| {
            Err(LlmError::Transport {
                message: "ptl mock script exhausted".into(),
            })
        })
    }
}

fn ptl_err(token_gap: u64) -> Result<HistoryResponse, LlmError> {
    Err(LlmError::ContextOverflow { token_gap })
}

// The `Result` wrap is required: `ok_text` is pushed into the same scripted
// response vec as `ptl_err` (which returns `Err`), so the type must match.
#[allow(clippy::unnecessary_wraps)]
fn ok_text(text: &str) -> Result<HistoryResponse, LlmError> {
    Ok(mock_message_response(
        vec![LlmContentBlock::Text {
            text: text.to_string(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    ))
}

struct SummaryClient {
    result: Result<&'static str, LlmError>,
    requests: std::sync::Mutex<Vec<sidequery::SideQueryRequest>>,
}

#[async_trait]
impl sidequery::SideQueryClient for SummaryClient {
    async fn query(
        &self,
        request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        self.requests.lock().unwrap().push(request);
        Ok(sidequery::SideQueryResponse {
            text: Some(
                self.result
                    .clone()
                    .map_err(sidequery::SideQueryError::Api)?
                    .into(),
            ),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn make_orch(
    api: Arc<PtlMockApi>,
    summary: Option<Arc<SummaryClient>>,
) -> (Arc<ConversationOrchestrator>, Arc<MockOutputStream>) {
    let output = Arc::new(MockOutputStream::new());
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    if let Some(summary) = summary {
        let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
        let runner = Arc::new(
            sidequery::ForkedAgentRunner::new()
                .with_side_query_client(summary, "test-model".into()),
        );
        orch = orch
            .with_cache_safe_slot(slot.clone())
            .with_compaction(Arc::new(CompactionOrchestrator::with_autocompactor(
                compaction::Autocompactor::with_forked_runner(runner, slot),
                u64::MAX,
            )));
    }
    (Arc::new(orch), output)
}

/// Seed `rounds` API rounds (`user, assistant(distinct id)` pairs) so the head
/// truncation has ≥2 groups to drop. Each message is padded so the token
/// estimate is meaningful.
async fn seed_rounds(orch: &ConversationOrchestrator, rounds: usize) {
    let session = orch.session();
    let mut s = session.lock().await;
    for i in 0..rounds {
        s.history.push(ConversationMessage::user(
            MessageId::new(),
            format!(
                "round-{i} user message with filler text to give the round a real token estimate"
            ),
        ));
        s.history.push(ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: format!(
                    "round-{i} assistant reply with filler text to give the round weight"
                ), citations: None,
            }],
            stop_reason: Some("end_turn".to_string()),
        });
    }
}

#[tokio::test]
async fn unwired_ptl_does_not_truncate_or_retry_the_main_request() {
    let api = Arc::new(PtlMockApi::new(vec![ptl_err(30)]));
    let (orch, output) = make_orch(api.clone(), None);
    seed_rounds(&orch, 20).await;
    let before = orch.session().lock().await.history.clone();
    orch.run_turn("trigger").await.unwrap();
    assert_eq!(api.call_lens().await.len(), 1);
    assert_eq!(orch.session().lock().await.history[..before.len()], before);
    assert!(output
        .snapshot()
        .await
        .iter()
        .any(|event| matches!(event, OutputEvent::Text { text } if text == "Prompt is too long")));
}

#[tokio::test]
async fn reactive_summary_success_retries_once_below_the_local_auto_threshold() {
    let api = Arc::new(PtlMockApi::new(vec![ptl_err(1), ok_text("recovered")]));
    let summary = Arc::new(SummaryClient {
        result: Ok("<summary>preserved work</summary>"),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let (orch, output) = make_orch(api.clone(), Some(summary.clone()));
    seed_rounds(&orch, 8).await;
    let oldest = orch.session().lock().await.history[0].clone();
    orch.run_turn("trigger").await.unwrap();
    assert_eq!(
        output.compaction_phase_snapshot().await,
        ["preparing", "summarizing", "restoring", "complete"]
    );
    assert_eq!(
        api.call_lens().await.len(),
        2,
        "one request after a successful summary"
    );
    {
        let requests = summary.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].messages.iter().find(|message| matches!(
                message,
                ConversationMessage::User { is_meta: false, .. }
            )),
            Some(&oldest),
            "the summary preserves the oldest user request after leading context"
        );
    }
    assert!(output
        .snapshot()
        .await
        .iter()
        .any(|event| matches!(event, OutputEvent::Text { text } if text == "recovered")));
    assert!(orch.session().lock().await.history.iter().any(|message| matches!(message, ConversationMessage::System { content, .. } if content == "Conversation compacted")));
}

#[tokio::test]
async fn failed_reactive_summary_keeps_all_original_messages_and_does_not_retry() {
    let api = Arc::new(PtlMockApi::new(vec![ptl_err(1)]));
    let summary = Arc::new(SummaryClient {
        result: Ok(" \n\t"),
        requests: std::sync::Mutex::new(Vec::new()),
    });
    let (orch, output) = make_orch(api.clone(), Some(summary.clone()));
    seed_rounds(&orch, 8).await;
    let before = orch.session().lock().await.history.clone();
    orch.run_turn("trigger").await.unwrap();
    assert_eq!(
        output.compaction_phase_snapshot().await,
        ["preparing", "summarizing", "error"]
    );
    assert_eq!(summary.requests.lock().unwrap().len(), 1);
    assert_eq!(api.call_lens().await.len(), 1);
    assert_eq!(orch.session().lock().await.history[..before.len()], before);
    assert!(output.snapshot().await.iter().any(|event| matches!(event, OutputEvent::Text { text } if text == "Prompt is too long · automatic compaction failed: summarization produced empty response")));
    assert!(!output
        .snapshot()
        .await
        .iter()
        .any(|event| matches!(event, OutputEvent::CompactionCompleted { .. })));
}
