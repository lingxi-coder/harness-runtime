//! Test fixtures.
//!
//! Gated behind `#[cfg(any(test, feature = "test-support"))]` so the
//! cli + tui crates can re-use the fixtures in M5-12 / M6 without
//! pulling them into release builds.

use crate::conversation::OrchestratorApiClient;
use async_trait::async_trait;
use lingxi_core::host::{CostSnapshot, OutputEvent, OutputStream};
use lingxi_core::types::ConversationMessage;
use llm_runtime::{
    ContentBlock as LlmContentBlock, ExecutionUsage as Usage, HistoryResponse, LlmError,
};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Attach the real forced-compaction pipeline with a deterministic summarizer.
/// Host integration tests can exercise transcript commits without adding
/// compaction/sidequery dependency edges or making provider calls.
pub fn with_scripted_compactor(
    orchestrator: crate::ConversationOrchestrator,
    summary: &str,
) -> crate::ConversationOrchestrator {
    struct SummaryClient(String);

    #[async_trait]
    impl sidequery::SideQueryClient for SummaryClient {
        async fn query(
            &self,
            _request: sidequery::SideQueryRequest,
        ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
            Ok(sidequery::SideQueryResponse {
                text: Some(format!("<summary>{}</summary>", self.0)),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    let slot = Arc::new(sidequery::CacheSafeParamsSlot::new());
    let runner = Arc::new(sidequery::ForkedAgentRunner::new().with_side_query_client(
        Arc::new(SummaryClient(summary.to_string())),
        "test-compact-model".to_string(),
    ));
    orchestrator
        .with_compaction(Arc::new(
            compaction::CompactionOrchestrator::with_autocompactor(
                compaction::Autocompactor::with_forked_runner(runner, slot.clone()),
                1_000,
            ),
        ))
        .with_cache_safe_slot(slot)
}

// ============================================================================
// MockApiClient (Task 6)
// ============================================================================

/// Scripted mock API client. Returns the responses queued at construction
/// time, in order. Captures each `msgs` argument for later assertion.
///
/// If the queue is exhausted, `messages_create` returns
/// `LlmError::Transport { message: "mock script exhausted" }` — synthetic
/// upstream failure so the orchestrator's max-turns guard is exercised.
pub struct MockApiClient {
    queue: Arc<Mutex<VecDeque<HistoryResponse>>>,
    captured_requests: Arc<Mutex<Vec<crate::OrchestratorApiRequest>>>,
    captured_models: Arc<Mutex<Vec<String>>>,
    captured_msgs: Arc<Mutex<Vec<Vec<ConversationMessage>>>>,
    captured_systems: Arc<Mutex<Vec<Option<String>>>>,
    captured_tools: Arc<Mutex<Vec<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>>>>,
    captured_prewarm: Arc<Mutex<Vec<MockPrewarmCall>>>,
    close_responses_ws_count: Arc<Mutex<u32>>,
    /// Explicit stream-fallback seeds; one entry per seeded request.
    captured_seeds: Arc<Mutex<Vec<u8>>>,
    /// Task 8 (llm-runtime future-work batch 3): the FULL internal rate-limit
    /// snapshot returned by `last_rate_limit_full()`. A `std::sync::Mutex`
    /// (not tokio) because the trait accessor is a sync `fn`.
    rate_limit_full: std::sync::Mutex<Option<crate::model::rate_limit::RateLimitInfo>>,
    /// Task 2 (llm-runtime future-work batch 5): the raw per-window snapshot
    /// returned by `last_raw_utilization()`. Same sync-Mutex rationale as
    /// `rate_limit_full`.
    raw_utilization: std::sync::Mutex<Option<crate::model::rate_limit::RawUtilization>>,
    /// Task 6 (llm-runtime future-work batch 5): when `Some`, every
    /// `messages_create` call fails with a clone of this error instead of
    /// consuming the queue — lets tests drive a terminal API failure (e.g.
    /// `LlmError::RateLimited`) through the turn loop.
    fail_with: std::sync::Mutex<Option<LlmError>>,
    /// Task 6 (batch 5): the composed limits copy returned by
    /// `last_rate_limit_error_message()`. Same sync-Mutex rationale as
    /// `rate_limit_full`.
    rate_limit_error_message: std::sync::Mutex<Option<String>>,
    /// The catalog returned by `list_model_listings()`. Empty by default (the
    /// trait default); tests that exercise provider-qualified model-ref parsing
    /// seed it with the rows they need.
    model_listings: std::sync::Mutex<Vec<lingxi_core::host::ModelListing>>,
    effort_snapshot: std::sync::Mutex<Option<lingxi_core::host::effort::EffortCommandSnapshot>>,
}

/// Captured startup Responses WebSocket prewarm call.
#[derive(Debug, Clone, PartialEq)]
pub struct MockPrewarmCall {
    /// Model id used for the prewarm request.
    pub model: String,
    /// Optional provider profile selected for the prewarm request.
    pub profile: Option<String>,
    /// Assembled system prompt sent to the provider.
    pub system: Option<String>,
    /// Conversation messages included in the prewarm request.
    pub messages: Vec<ConversationMessage>,
    /// Wire tool schemas included in the prewarm request.
    pub tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    /// Native per-request policy flag for global system-prompt caching.
    pub skip_global_cache_for_system_prompt: bool,
}

impl MockApiClient {
    /// Supply authoritative native selected-route state. Synthetic mock routes
    /// otherwise use the caller's explicit model identity, without native settings.
    pub fn set_effort_command_snapshot(
        &self,
        snapshot: Option<lingxi_core::host::effort::EffortCommandSnapshot>,
    ) {
        *self.effort_snapshot.lock().unwrap() = snapshot;
    }

    /// Construct a mock with a script of `responses` returned in order.
    #[must_use]
    pub fn new(responses: Vec<HistoryResponse>) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::from(responses))),
            captured_requests: Arc::new(Mutex::new(Vec::new())),
            captured_models: Arc::new(Mutex::new(Vec::new())),
            captured_msgs: Arc::new(Mutex::new(Vec::new())),
            captured_systems: Arc::new(Mutex::new(Vec::new())),
            captured_tools: Arc::new(Mutex::new(Vec::new())),
            captured_prewarm: Arc::new(Mutex::new(Vec::new())),
            close_responses_ws_count: Arc::new(Mutex::new(0)),
            captured_seeds: Arc::new(Mutex::new(Vec::new())),
            rate_limit_full: std::sync::Mutex::new(None),
            raw_utilization: std::sync::Mutex::new(None),
            fail_with: std::sync::Mutex::new(None),
            rate_limit_error_message: std::sync::Mutex::new(None),
            model_listings: std::sync::Mutex::new(Vec::new()),
            effort_snapshot: std::sync::Mutex::new(None),
        }
    }

    /// Seed the catalog `list_model_listings()` returns, so a test can exercise
    /// `lingxi_core::host::parse_model_ref` (which resolves a `profile/model` reference
    /// only against real listings).
    pub fn set_model_listings(&self, listings: Vec<lingxi_core::host::ModelListing>) {
        *self.model_listings.lock().unwrap() = listings;
    }

    /// Task 6 (batch 5): make every subsequent `messages_create` fail with a
    /// clone of `err` (the queue is bypassed). Pass `None` to restore the
    /// scripted-queue behaviour.
    pub fn set_fail_with(&self, err: Option<LlmError>) {
        *self.fail_with.lock().unwrap() = err;
    }

    /// Task 6 (batch 5): pre-load the composed limits copy returned by
    /// `last_rate_limit_error_message()`. Pass `None` to clear it (the
    /// default).
    pub fn set_rate_limit_error_message(&self, msg: Option<String>) {
        *self.rate_limit_error_message.lock().unwrap() = msg;
    }

    /// Task 8: pre-load the FULL internal rate-limit snapshot returned by
    /// `last_rate_limit_full()`. Pass `None` to clear it (the default).
    /// Synchronous so tests can flip the value between `run_turn` calls
    /// without an `await`.
    pub fn set_rate_limit_full(&self, info: Option<crate::model::rate_limit::RateLimitInfo>) {
        *self.rate_limit_full.lock().unwrap() = info;
    }

    /// Task 2 (batch 5): pre-load the raw per-window snapshot returned by
    /// `last_raw_utilization()`. Pass `None` to clear it (the default).
    /// Synchronous for the same between-turns flipping reason as
    /// [`Self::set_rate_limit_full`].
    pub fn set_raw_utilization(&self, raw: Option<crate::model::rate_limit::RawUtilization>) {
        *self.raw_utilization.lock().unwrap() = raw;
    }

    /// Snapshot the captured `tools` arguments (one entry per `messages_create`
    /// call). Lets a test assert the orchestrator advertised the registry's
    /// wire tool definitions on the batched path.
    pub async fn captured_tools(&self) -> Vec<Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>> {
        self.captured_tools.lock().await.clone()
    }

    /// Snapshot captured startup Responses WebSocket prewarm calls.
    pub async fn captured_prewarm(&self) -> Vec<MockPrewarmCall> {
        self.captured_prewarm.lock().await.clone()
    }

    /// Number of times the test client was asked to close the Responses
    /// WebSocket session.
    pub async fn close_responses_ws_count(&self) -> u32 {
        *self.close_responses_ws_count.lock().await
    }

    /// Snapshot complete owned requests, including call policy and options.
    pub async fn captured_requests(&self) -> Vec<crate::OrchestratorApiRequest> {
        self.captured_requests.lock().await.clone()
    }

    /// Snapshot the captured `msgs` arguments (one entry per `messages_create` call).
    pub async fn captured_msgs(&self) -> Vec<Vec<ConversationMessage>> {
        self.captured_msgs.lock().await.clone()
    }

    pub async fn captured_models(&self) -> Vec<String> {
        self.captured_models.lock().await.clone()
    }

    /// Snapshot the captured `system` arguments (one entry per call;
    /// `None` for calls that passed no system prompt). Added M5-03 to
    /// support prompt-wiring assertions.
    pub async fn captured_systems(&self) -> Vec<Option<String>> {
        self.captured_systems.lock().await.clone()
    }

    /// Explicit stream-fallback seeds, including zero.
    /// Ordinary main requests do not add an entry.
    pub async fn captured_seeds(&self) -> Vec<u8> {
        self.captured_seeds.lock().await.clone()
    }

    /// Number of responses still queued.
    pub async fn remaining(&self) -> usize {
        self.queue.lock().await.len()
    }
}

#[async_trait]
impl OrchestratorApiClient for MockApiClient {
    fn effort_command_snapshot(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, LlmError> {
        Ok(self.effort_snapshot.lock().unwrap().clone())
    }

    async fn messages_create(
        &self,
        request: crate::OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        self.captured_requests.lock().await.push(request.clone());
        if let crate::OrchestratorApiRequest::Main(request) = &request {
            if let Some(seed) = request.opts.initial_consecutive_overloaded {
                self.captured_seeds.lock().await.push(seed);
            }
        }

        let (request_model, request_profile, request_system, msgs, tools) = match request {
            crate::OrchestratorApiRequest::Main(request) => (
                request.model,
                request.profile,
                request.system.map(|system| system.display_text()),
                request.messages,
                request.tools,
            ),
            crate::OrchestratorApiRequest::HookPrompt(request) => (
                request.model,
                request.profile,
                Some(request.system),
                request.messages,
                Vec::new(),
            ),
        };
        let _model = request_model.as_str();
        let _profile = request_profile.as_deref();
        let system = request_system.as_deref();

        self.captured_models
            .lock()
            .await
            .push(request_model.clone());
        self.captured_msgs.lock().await.push(msgs);
        self.captured_systems
            .lock()
            .await
            .push(system.map(str::to_string));
        self.captured_tools.lock().await.push(tools);
        // Task 6 (batch 5): scripted failure wins over the queue.
        if let Some(err) = self.fail_with.lock().unwrap().clone() {
            return Err(err);
        }
        let mut q = self.queue.lock().await;
        q.pop_front().ok_or_else(|| LlmError::Transport {
            message: "mock script exhausted".into(),
        })
    }

    /// Task 8: return the snapshot pre-loaded via [`Self::set_rate_limit_full`].
    fn last_rate_limit_full(&self) -> Option<crate::model::rate_limit::RateLimitInfo> {
        self.rate_limit_full.lock().unwrap().clone()
    }

    /// Task 2 (batch 5): return the snapshot pre-loaded via
    /// [`Self::set_raw_utilization`].
    fn last_raw_utilization(&self) -> Option<crate::model::rate_limit::RawUtilization> {
        *self.raw_utilization.lock().unwrap()
    }

    /// Task 6 (batch 5): return the copy pre-loaded via
    /// [`Self::set_rate_limit_error_message`].
    fn last_rate_limit_error_message(&self) -> Option<String> {
        self.rate_limit_error_message.lock().unwrap().clone()
    }

    async fn prewarm_responses_websocket(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<lingxi_core::types::utf16_json::Utf16JsonProjection>,
        skip_global_cache_for_system_prompt: bool,
    ) -> Result<(), LlmError> {
        self.captured_prewarm.lock().await.push(MockPrewarmCall {
            model: model.to_string(),
            profile: profile.map(str::to_string),
            system: system.map(|system| system.display_text()),
            messages,
            tools,
            skip_global_cache_for_system_prompt,
        });
        Ok(())
    }

    async fn close_responses_websocket_session(&self) -> Result<(), LlmError> {
        *self.close_responses_ws_count.lock().await += 1;
        Ok(())
    }

    /// The catalog seeded by [`MockApiClient::set_model_listings`] (empty by
    /// default, matching the trait's own default).
    fn list_model_listings(&self) -> Vec<lingxi_core::host::ModelListing> {
        self.model_listings.lock().unwrap().clone()
    }
}

/// Tiny helper for tests to construct a fully populated `HistoryResponse`
/// without typing out every field. Defaults: zero usage, no thinking,
/// caller picks the content blocks + `stop_reason`.
#[must_use]
pub fn mock_message_response(
    content: Vec<LlmContentBlock>,
    stop_reason: Option<&str>,
) -> HistoryResponse {
    HistoryResponse {
        id: "msg_mock".to_string(),
        model: "claude-opus-4-7".to_string(),
        content,
        stop_reason: stop_reason.map(str::to_string),
        stop_details: None,
        usage: Usage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

// ============================================================================
// MockOutputStream (Task 7)
// ============================================================================

/// Capture all `OutputStream` events into an in-memory `Vec` for assertion.
/// `Clone` shares the same underlying event buffer (`Arc<Mutex<…>>`), so a
/// cloned handle observes events emitted through any clone — used to inspect
/// the orchestrator's emitted output after wrapping the stream in an `Arc`.
#[derive(Clone)]
pub struct MockOutputStream {
    partial_stream_events: Arc<Mutex<Vec<String>>>,
    include_partial_stream_events: bool,
    lifecycle_events: Arc<Mutex<Vec<serde_json::Value>>>,
    model_fallback_frames: Arc<Mutex<Vec<serde_json::Value>>>,
    events: Arc<Mutex<Vec<OutputEvent>>>,
    /// Denial provenance observed via `emit_tool_result_denied`, as
    /// `(tool_use_id, denial_kind)` in emission order.
    ///
    /// Kept OUT of [`OutputEvent`] because denial details are a test-only
    /// diagnostic, not a user-facing output event. The trait method is
    /// DEFAULTED, so without an explicit override the mock would silently drop
    /// `denial_kind` and let every deny-path test pass regardless of the value.
    denials: Arc<Mutex<Vec<(lingxi_core::types::ToolUseId, String)>>>,
    /// Attachments observed via `emit_attachment`. The trait method is
    /// DEFAULTED, so without this override the mock would
    /// inherit the no-op and every attachment test would pass whether or not
    /// the orchestrator emitted anything.
    attachments: Arc<Mutex<Vec<lingxi_core::host::AttachmentKind>>>,
    /// Compaction lifecycle is separate from transcript events.
    compaction_phases: Arc<Mutex<Vec<String>>>,
    /// How many times `emit_turn_started` was called, same rationale as
    /// `denials` and `attachments`: the trait method is DEFAULTED, so without
    /// this override the mock inherits the no-op and a test cannot tell an
    /// announced turn from an unannounced one. That is not hypothetical — the
    /// bridge's own `AdapterOutputStream` shipped without the override, so
    /// `ClientEvent::TurnStarted` had no producer at all and every turn the
    /// engine started by itself ran invisibly on Desktop.
    turn_starts: Arc<Mutex<usize>>,
}

impl MockOutputStream {
    pub async fn lifecycle_event_snapshot(&self) -> Vec<serde_json::Value> {
        self.lifecycle_events.lock().await.clone()
    }

    pub async fn model_fallback_frame_snapshot(&self) -> Vec<serde_json::Value> {
        self.model_fallback_frames.lock().await.clone()
    }

    /// Construct an empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self {
            partial_stream_events: Arc::new(Mutex::new(Vec::new())),
            include_partial_stream_events: false,
            events: Arc::new(Mutex::new(Vec::new())),
            lifecycle_events: Arc::new(Mutex::new(Vec::new())),
            model_fallback_frames: Arc::new(Mutex::new(Vec::new())),
            denials: Arc::new(Mutex::new(Vec::new())),
            attachments: Arc::new(Mutex::new(Vec::new())),
            compaction_phases: Arc::new(Mutex::new(Vec::new())),
            turn_starts: Arc::new(Mutex::new(0)),
        }
    }

    #[must_use]
    pub fn with_partial_stream_events(mut self) -> Self {
        self.include_partial_stream_events = true;
        self
    }

    pub async fn partial_stream_event_snapshot(&self) -> Vec<String> {
        self.partial_stream_events.lock().await.clone()
    }

    /// How many times the orchestrator announced a turn the client did not
    /// submit (`emit_turn_started`).
    pub async fn turn_start_count(&self) -> usize {
        *self.turn_starts.lock().await
    }

    /// Snapshot observed compaction phases, including start and terminal status.
    pub async fn compaction_phase_snapshot(&self) -> Vec<String> {
        self.compaction_phases.lock().await.clone()
    }

    /// Snapshot the attachments emitted so far, in emission order.
    pub async fn attachment_snapshot(&self) -> Vec<lingxi_core::host::AttachmentKind> {
        self.attachments.lock().await.clone()
    }

    /// Snapshot the `(tool_use_id, denial_kind)` pairs captured so far.
    pub async fn denial_snapshot(&self) -> Vec<(lingxi_core::types::ToolUseId, String)> {
        self.denials.lock().await.clone()
    }

    /// Snapshot the captured events.
    pub async fn snapshot(&self) -> Vec<OutputEvent> {
        self.events.lock().await.clone()
    }

    /// Convenience: text events in capture order.
    pub async fn text_events(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .filter_map(|e| match e {
                OutputEvent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Convenience: tool-call events in capture order.
    pub async fn tool_calls(&self) -> Vec<(String, serde_json::Value)> {
        self.events
            .lock()
            .await
            .iter()
            .filter_map(|e| match e {
                OutputEvent::ToolCall { tool, input, .. } => Some((tool.clone(), input.clone())),
                _ => None,
            })
            .collect()
    }

    /// Convenience: terminal-sequence events in capture order (#6).
    pub async fn terminal_sequences(&self) -> Vec<String> {
        self.events
            .lock()
            .await
            .iter()
            .filter_map(|e| match e {
                OutputEvent::TerminalSequence { seq } => Some(seq.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Default for MockOutputStream {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OutputStream for MockOutputStream {
    fn wants_partial_stream_events(&self) -> bool {
        self.include_partial_stream_events
    }

    async fn emit_stream_event(&self, event_json: &str, _is_message_start: bool) {
        self.partial_stream_events
            .lock()
            .await
            .push(event_json.to_string());
    }
    async fn emit_model_fallback(
        &self,
        id: &lingxi_core::types::MessageId,
        session_id: &lingxi_core::types::SessionId,
        content: &str,
        metadata: &lingxi_core::types::ModelFallbackMetadata,
    ) {
        self.emit_system_notice(content, false).await;
        self.model_fallback_frames
            .lock()
            .await
            .push(metadata.sdk_frame(*id, *session_id, content));
    }

    async fn emit_task_lifecycle(&self, event: &serde_json::Value) {
        self.lifecycle_events.lock().await.push(event.clone());
    }

    async fn emit_turn_started(&self) {
        *self.turn_starts.lock().await += 1;
    }

    async fn emit_assistant_message_identity(&self, message_id: &lingxi_core::types::MessageId) {
        self.events.lock().await.push(OutputEvent::MessageIdentity {
            message_id: *message_id,
        });
    }

    async fn emit_user_transcript_row_identity(&self, row_token: &str, uuid: &str) {
        self.events
            .lock()
            .await
            .push(OutputEvent::UserTranscriptRowIdentity {
                row_token: row_token.to_string(),
                uuid: uuid.to_string(),
            });
    }

    async fn emit_assistant_transcript_row_uuids(
        &self,
        message_id: &lingxi_core::types::MessageId,
        uuids: &[Option<String>],
    ) {
        self.events
            .lock()
            .await
            .push(OutputEvent::AssistantTranscriptRowUuids {
                message_id: *message_id,
                uuids: uuids.to_vec(),
            });
    }
    async fn emit_message_retracted(&self, message_id: &lingxi_core::types::MessageId) {
        self.events
            .lock()
            .await
            .push(OutputEvent::MessageRetracted {
                message_id: *message_id,
            });
    }
    async fn emit_text(&self, text: &str, _utf16_code_units: Option<&[u16]>) {
        self.events.lock().await.push(OutputEvent::Text {
            text: text.to_string(),
        });
    }
    async fn emit_system_notice(&self, body: &str, is_error: bool) {
        self.events.lock().await.push(OutputEvent::SystemNotice {
            body: body.to_string(),
            is_error,
        });
    }
    async fn emit_mod_log(&self, plugin: &str, text: &str) {
        self.events.lock().await.push(OutputEvent::ModLog {
            plugin: plugin.to_string(),
            text: text.to_string(),
        });
    }

    async fn emit_mod_toast(&self, plugin: &str, text: &str, timeout_ms: u64) {
        self.events.lock().await.push(OutputEvent::ModToast {
            plugin: plugin.to_string(),
            text: text.to_string(),
            timeout_ms,
        });
    }

    async fn emit_mod_status(&self, plugin: &str, text: Option<&str>) {
        self.events.lock().await.push(OutputEvent::ModStatus {
            plugin: plugin.to_string(),
            text: text.map(str::to_string),
        });
    }
    async fn emit_terminal_sequence(&self, seq: &str) {
        self.events
            .lock()
            .await
            .push(OutputEvent::TerminalSequence {
                seq: seq.to_string(),
            });
    }
    async fn emit_tool_call(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        input: &serde_json::Value,
     _input_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>) {
        self.events.lock().await.push(OutputEvent::ToolCall {
            id: id.clone(),
            tool: tool.to_string(),
            input: input.clone(),
        });
    }
    async fn emit_tool_heartbeat(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        elapsed_ms: u64,
    ) {
        self.events.lock().await.push(OutputEvent::ToolHeartbeat {
            id: id.clone(),
            tool: tool.to_string(),
            elapsed_ms,
        });
    }
    async fn emit_tool_result(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        _model_text: &str,
        result: &serde_json::Value,
     _projection: Option<&lingxi_core::host::ToolResultProjection>) {
        self.events.lock().await.push(OutputEvent::ToolResult {
            id: id.clone(),
            tool: tool.to_string(),
            result: result.clone(),
        });
    }
    async fn emit_attachment(&self, attachment: lingxi_core::host::AttachmentKind) {
        self.attachments.lock().await.push(attachment);
    }

    async fn emit_tool_result_denied(
        &self,
        id: &lingxi_core::types::ToolUseId,
        tool: &str,
        model_text: &str,
        result: &serde_json::Value,
        denial_kind: &str,
     _projection: Option<&lingxi_core::host::ToolResultProjection>) {
        self.denials
            .lock()
            .await
            .push((id.clone(), denial_kind.to_string()));
        // Still record the ordinary result event so existing assertions that
        // count/inspect `ToolResult` keep seeing denied tools.
        self.emit_tool_result(id, tool, model_text, result, None).await;
    }
    async fn emit_end_turn(&self, stop_reason: &str, cost: &CostSnapshot) {
        self.events.lock().await.push(OutputEvent::EndTurn {
            stop_reason: stop_reason.to_string(),
            cost: cost.clone(),
        });
    }
    async fn emit_compaction_started(&self) {
        self.emit_compaction_phase("preparing").await;
    }

    async fn emit_compaction_phase(&self, phase: &str) {
        self.compaction_phases.lock().await.push(phase.to_string());
    }

    async fn emit_compaction_skipped(&self) {
        self.emit_compaction_phase("skipped").await;
    }

    async fn emit_compaction_finished(&self, error: Option<&str>) {
        self.emit_compaction_phase(match error {
            None => "complete",
            Some("Compaction canceled.") => "cancelled",
            Some(_) => "error",
        })
        .await;
    }

    async fn emit_compaction_completed(
        &self,
        messages_before: u32,
        messages_after: u32,
        bytes_saved: u64,
        summary: &str,
    ) {
        self.events
            .lock()
            .await
            .push(OutputEvent::CompactionCompleted {
                messages_before,
                messages_after,
                bytes_saved,
                summary: summary.to_string(),
            });
    }
    async fn emit_thinking(&self, thinking: &str, signature: Option<&str>) {
        self.events.lock().await.push(OutputEvent::Thinking {
            thinking: thinking.to_string(),
            signature: signature.map(str::to_string),
        });
    }
    async fn emit_usage(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) {
        self.events.lock().await.push(OutputEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        });
    }
    /// Task 8 (llm-runtime future-work batch 3): record the rate-limit
    /// emission so tests can assert the emit-on-change behaviour.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the eleven-argument trait signature (see lingxi_core::host::OutputStream::emit_rate_limit)"
    )]
    async fn emit_rate_limit(
        &self,
        status: Option<&str>,
        rate_limit_type: Option<&str>,
        utilization: Option<f64>,
        resets_at: Option<u64>,
        claim_resets_at: Option<u64>,
        overage_status: Option<&str>,
        overage_resets_at: Option<u64>,
        overage_disabled_reason: Option<&str>,
        fallback_available: Option<bool>,
        upgrade_paths: Option<&[String]>,
        credits_required: bool,
    ) {
        self.events.lock().await.push(OutputEvent::RateLimit {
            status: status.map(str::to_string),
            rate_limit_type: rate_limit_type.map(str::to_string),
            utilization,
            resets_at,
            claim_resets_at,
            overage_status: overage_status.map(str::to_string),
            overage_resets_at,
            overage_disabled_reason: overage_disabled_reason.map(str::to_string),
            fallback_available,
            upgrade_paths: upgrade_paths.map(<[String]>::to_vec),
            credits_required,
        });
    }
    /// Task 2 (llm-runtime future-work batch 5): record the raw-utilization
    /// emission so tests can assert the emit-on-change behaviour.
    async fn emit_raw_utilization(
        &self,
        five_hour_utilization: Option<f64>,
        five_hour_resets_at: Option<u64>,
        seven_day_utilization: Option<f64>,
        seven_day_resets_at: Option<u64>,
    ) {
        self.events.lock().await.push(OutputEvent::RawUtilization {
            five_hour_utilization,
            five_hour_resets_at,
            seven_day_utilization,
            seven_day_resets_at,
        });
    }
}

// ============================================================================
// HookExecutor + PermissionGate stubs (Task 8)
// ============================================================================
//
// These local traits will be renamespaced or replaced by M5-05 (real
// PermissionGate) and M5-06 (real 4-arm HookExecutor). M5-02 ships
// allow-all stubs against minimal trait surfaces so the orchestrator can
// be constructed in tests without dragging in the full hooks/permission
// machinery.

// M5-06 Task 14: the local `HookExecutor` trait that M5-02 introduced is
// replaced by the real `hooks::HookExecutorImpl`. We re-export the
// concrete type so existing imports (crate::test_support::HookExecutor)
// keep working as a type alias.
pub use hooks::HookExecutorImpl as HookExecutor;

/// Construct an empty `HookExecutorImpl` suitable for tests + the
/// orchestrator's "no hooks configured" path. The registry is empty so
/// `execute()` always returns a fresh `AggregateHookResult::default()`
/// without ever calling the supplied http/runtime stubs.
///
/// M5-06 Task 14: replaces the M5-02 `NoOpHookExecutor` unit struct so
/// the orchestrator can carry an `Arc<HookExecutorImpl>` instead of an
/// `Arc<dyn local::HookExecutor>` trait object.
#[must_use]
pub fn noop_hook_executor() -> Arc<hooks::HookExecutorImpl> {
    use hooks::registry::HookRegistry;

    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "noop hook executor — http arm is never called with an empty registry".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "noop hook executor — sse arm is never called".into(),
            ))
        }
    }

    struct UnusedRuntime;
    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal(
                "noop hook executor — runtime arm is never called".into(),
            ))
        }
        async fn sleep(&self, _duration: std::time::Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    let registry = Arc::new(tokio::sync::RwLock::new(HookRegistry::new()));
    let http: Arc<dyn lingxi_core::host::HttpTransport> = Arc::new(UnusedHttp);
    let runtime: Arc<dyn lingxi_core::host::RuntimeSpawner> = Arc::new(UnusedRuntime);
    Arc::new(hooks::HookExecutorImpl::new(registry, http, runtime))
}

// M5-06 Task 14: M5-02's `pub struct NoOpHookExecutor;` is gone — the
// orchestrator now carries `Arc<HookExecutorImpl>` directly. Call sites
// previously using `Arc::new(NoOpHookExecutor)` should now call
// `crate::test_support::noop_hook_executor()` (which returns the Arc
// directly).

// M5-05 Task 2: PermissionGate + PermissionDecision are promoted to
// lingxi-lingxi_core::host::permission_gate. We re-export them here so existing
// orchestrator imports (crate::test_support::PermissionGate, …) keep
// working unchanged.
pub use permission::gate::{
    PermissionDecision, PermissionDecisionSource, PermissionGate, PermissionResolution,
};

/// Allow-all permission gate. Always returns `Allow`.
///
/// **M5-05:** the trait surface moved to `lingxi-traits` but the impl
/// stays here for back-compat with M5-02 / M5-04 tests that import
/// `crate::test_support::NoOpPermissionGate`. Production composition chooses
/// between this no-op and [`permission::InteractivePromptingGate`] before
/// constructing the orchestrator; the separate
/// [`crate::OrchestratorConfig::interactive_session`] flag controls
/// prompt/request interactivity.
pub struct NoOpPermissionGate;

#[async_trait]
impl PermissionGate for NoOpPermissionGate {
    async fn check(&self, _tool_name: &str, _input: &serde_json::Value) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

// ============================================================================
// StaticMemoryProvider (Task 9)
// ============================================================================

/// Test fixture: returns a fixed `Vec<MemoryFile>` regardless of cwd.
/// Used by the prompt-wiring integration tests in M5-03 so they can
/// drive the orchestrator without touching the filesystem.
pub struct StaticMemoryProvider {
    files: Vec<crate::prompt::MemoryFile>,
}

impl StaticMemoryProvider {
    /// Empty fixture — `load()` always returns `vec![]`.
    #[must_use]
    pub fn empty() -> Self {
        Self { files: Vec::new() }
    }

    /// Pre-loaded fixture — `load()` always returns the provided files.
    #[must_use]
    pub fn with_files(files: Vec<crate::prompt::MemoryFile>) -> Self {
        Self { files }
    }
}

#[async_trait]
impl crate::prompt::MemoryHierarchyProvider for StaticMemoryProvider {
    async fn load(&self, _cwd: &std::path::Path) -> Vec<crate::prompt::MemoryFile> {
        self.files.clone()
    }
    async fn load_conditional_rules(
        &self,
        cwd: &std::path::Path,
        trigger: &std::path::Path,
        mode: crate::prompt::memory_block::InstructionFilesMode,
    ) -> Vec<crate::prompt::MemoryFile> {
        self.files
            .iter()
            .filter(|file| {
                file.globs.is_some()
                    && (mode != crate::prompt::memory_block::InstructionFilesMode::ManagedOnly
                        || file.tier == memory::lingxi_md::LingxiMdTier::Managed)
                    && crate::prompt::conditional_rules::rule_matches_touched_file(
                        file, trigger, cwd,
                    )
            })
            .cloned()
            .collect()
    }
}

// ============================================================================
// Streaming-path test fixtures (re-exports — M5-04)
// ============================================================================
//
// `test_support_stream` is the home of `MockStreamingApiClient`,
// `MockToolDispatchClock`, the per-event helpers (`message_start`,
// `text_delta`, …) and the `scripted!` macro. Re-export them through
// the `test_support` namespace so integration tests can `use
// orchestrator::test_support::{MockStreamingApiClient, …}`
// without importing two distinct modules.

pub use crate::test_support_stream::{
    content_block_start_text, content_block_start_thinking, content_block_start_tool_use,
    content_block_stop, input_json_delta, message_delta_stop, message_delta_stop_with_usage,
    message_start, message_stop, ping, text_delta, thinking_delta, MockStreamingApiClient,
    MockToolDispatchClock,
};

// ============================================================================
// MockOrchestratorHandle (M5-10 Task 2)
// ============================================================================
//
// Scripted mock of `lingxi_core::host::OrchestratorHandle` for the M5-10/M5-11
// slash-command handler tests. Captures every call as a flag/counter and
// returns whatever the test pre-loaded via setter methods.

use lingxi_core::host::{
    ActiveGoalSnapshot, AgentInfo, CompactionSummary, DoctorReport, HandleError, HookInfo,
    McpServerInfo, MemoryEditorOutcome, OrchestratorHandle, SkillInfo, StatusSnapshot,
};
use lingxi_core::types::SessionId;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

type EffortCommandSource = std::sync::Arc<
    dyn Fn(
            &str,
            lingxi_core::host::effort_table::SessionEffort,
        ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, HandleError>
        + Send
        + Sync,
>;

/// Test double for `OrchestratorHandle`.
///
/// Defaults: `current_session_id` returns a stable v4 UUID; all mutators
/// return `Ok(())` (or appropriate success defaults); flags are recorded
/// for later assertion via `was_*_called()` accessors.
pub struct MockOrchestratorHandle {
    /// Stable session id returned by `current_session_id`.
    session_id: SessionId,
    /// Number of `clear_session` calls.
    clear_calls: AtomicUsize,
    /// If `Some`, `clear_session` returns `ActionFailed(_)` instead of `Ok(())`.
    ///
    /// Uses `std::sync::Mutex` (NOT `tokio::sync::Mutex`) so test code can
    /// set the value synchronously without an `await` and without
    /// `blocking_lock()` (which would panic inside the tokio runtime).
    clear_error: StdMutex<Option<String>>,
    /// Pre-loaded `CompactionSummary` returned by `force_compact`. If not
    /// set, defaults to `CompactionSummary::default()`.
    compact_summary: StdMutex<Option<CompactionSummary>>,
    /// If `Some`, `force_compact` returns `ActionFailed(_)`.
    compact_error: StdMutex<Option<String>>,
    /// The `custom_instructions` most recently passed to
    /// `force_compact_with_instructions` (`/compact <focus>` forwarding proof).
    compact_instructions: StdMutex<Option<String>>,
    /// Bumped each `switch_model` call. Records the most-recent value too.
    switch_model_calls: AtomicUsize,
    switch_model_last: StdMutex<Option<String>>,
    /// Most-recent profile passed to `switch_model`, or `None`.
    switch_model_last_profile: StdMutex<Option<Option<String>>>,
    /// If `Some`, `switch_model` returns `ActionFailed(_)`.
    switch_model_error: StdMutex<Option<String>>,
    /// Live permission-mode wire id used by bridge/mobile routing tests.
    permission_mode: StdMutex<Option<String>>,
    /// Live effort value used by `/effort` command tests.
    effort: StdMutex<Option<String>>,
    effort_session: StdMutex<lingxi_core::host::effort_table::SessionEffort>,
    effort_command_source: StdMutex<Option<EffortCommandSource>>,
    reasoning_default_path: StdMutex<Option<PathBuf>>,
    /// Output-style listing returned by `output_styles`; `None` means this
    /// engine has no prompt-assembly layer (the trait default).
    output_style_listing: StdMutex<Option<lingxi_core::host::OutputStyleListing>>,
    /// Every name passed to `set_output_style`, in order.
    output_style_switches: StdMutex<Vec<String>>,
    /// Optional live controls snapshot for routing synchronization tests.
    conversation_controls: StdMutex<Option<lingxi_core::host::ConversationControls>>,
    /// Session-scoped fast-mode flag used by bridge routing tests.
    fast_mode: AtomicBool,
    ultracode_enabled: AtomicBool,
    /// Session-owned dynamic-workflow gate exposed through the handle.
    dynamic_workflows_gate: lingxi_core::host::session_flags::DynamicWorkflowsGate,
    /// Session-owned workflow-size state exposed through the handle.
    workflow_size_guideline: lingxi_core::host::session_flags::WorkflowSizeGuidelineState,
    /// If `Some`, the next `set_permission_mode` call returns `ActionFailed(_)`.
    permission_mode_error: StdMutex<Option<String>>,
    /// Set by `request_exit`. Readable via `was_exit_requested`.
    exit_requested: AtomicBool,
    /// Pre-loaded path for `open_memory_editor`.
    memory_path: StdMutex<Option<PathBuf>>,
    /// Pre-loaded exit code for `open_memory_editor`.
    editor_exit_code: AtomicI32,
    /// If `Some`, `open_memory_editor` returns `ActionFailed(_)`.
    memory_error: StdMutex<Option<String>>,
    /// Cost snapshot fields (rarely exercised in M5-10).
    cost_nano_usd: AtomicU64,
    cost_tokens: AtomicU64,
    /// Optional pre-loaded full cost snapshot returned by `snapshot_cost`.
    /// If `Some`, used verbatim (with `session_id` overwritten to mock's id).
    cost_snapshot: StdMutex<Option<lingxi_core::host::CostSnapshot>>,
    // M5-11 additions:
    /// Pre-loaded MCP server list returned by `list_mcp_servers`.
    mcp_servers: StdMutex<Vec<McpServerInfo>>,
    /// Pre-loaded skill list returned by `list_skills`.
    skills: StdMutex<Vec<SkillInfo>>,
    /// Pre-loaded hooks list returned by `list_hooks`.
    hooks_list: StdMutex<Vec<HookInfo>>,
    /// Session-scoped `/goal` state returned through the handle.
    active_goal: StdMutex<Option<ActiveGoalSnapshot>>,
    /// `/goal` trust gate.
    workspace_trusted: AtomicBool,
    /// `/goal` hooks-restricted gate.
    hooks_restricted: AtomicBool,
    /// Pre-loaded agents list returned by `list_agents`.
    agents_list: StdMutex<Vec<AgentInfo>>,
    /// Pre-loaded doctor report returned by `run_doctor_checks`.
    doctor_report: StdMutex<DoctorReport>,
    /// Pre-loaded status snapshot returned by `get_status_snapshot`.
    status_snapshot: StdMutex<StatusSnapshot>,
    /// If `Some`, `edit_config_file` returns `ActionFailed(_)`.
    config_editor_error: StdMutex<Option<String>>,
    /// If `Some`, `edit_permissions_file` returns `ActionFailed(_)`.
    permissions_editor_error: StdMutex<Option<String>>,
    /// Pre-loaded available models list returned by `list_available_models`.
    available_models: StdMutex<Vec<String>>,
    /// Pre-loaded read-file-state cache keys returned by `files_in_context`.
    files_in_context: StdMutex<Vec<PathBuf>>,
    /// Pre-loaded model listings returned by `list_model_listings`.
    model_listings: StdMutex<Vec<lingxi_core::host::ModelListing>>,
    /// Local slash-command transcript pairs requested by a host.
    slash_command_transcript: StdMutex<Vec<(String, String)>>,
    mod_describe_inputs: StdMutex<Vec<serde_json::Value>>,
    mod_describe_rewrite: StdMutex<Option<(String, String, Option<String>, bool)>>,
    /// Every `body` passed to `emit_background_system_notice`, in call order
    /// (WP6/F006: the Fusion completion sink's best-effort UI notice).
    background_notices: StdMutex<Vec<String>>,
}

impl MockOrchestratorHandle {
    /// Supply the explicit session input source used by command tests.
    pub fn set_effort_command_source<F>(&self, source: F)
    where
        F: Fn(
                &str,
                lingxi_core::host::effort_table::SessionEffort,
            )
                -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, HandleError>
            + Send
            + Sync
            + 'static,
    {
        *self.effort_command_source.lock().unwrap() = Some(std::sync::Arc::new(source));
    }
    /// Supply an admitted default path without ambient lookup.
    pub fn set_reasoning_default_settings_path(&self, path: Option<PathBuf>) {
        *self.reasoning_default_path.lock().unwrap() = path;
    }

    /// Construct a fresh mock with sane defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            session_id: SessionId::new(),
            clear_calls: AtomicUsize::new(0),
            clear_error: StdMutex::new(None),
            compact_summary: StdMutex::new(None),
            compact_instructions: StdMutex::new(None),
            compact_error: StdMutex::new(None),
            switch_model_calls: AtomicUsize::new(0),
            switch_model_last: StdMutex::new(None),
            switch_model_last_profile: StdMutex::new(None),
            switch_model_error: StdMutex::new(None),
            permission_mode: StdMutex::new(Some("default".to_string())),
            effort: StdMutex::new(None),
            effort_session: StdMutex::new(Default::default()),
            effort_command_source: StdMutex::new(None),
            reasoning_default_path: StdMutex::new(None),
            output_style_listing: StdMutex::new(None),
            output_style_switches: StdMutex::new(Vec::new()),
            conversation_controls: StdMutex::new(None),
            fast_mode: AtomicBool::new(false),
            ultracode_enabled: AtomicBool::new(false),
            dynamic_workflows_gate: lingxi_core::host::session_flags::DynamicWorkflowsGate::new(
                false, false,
            ),
            workflow_size_guideline:
                lingxi_core::host::session_flags::WorkflowSizeGuidelineState::default(),
            permission_mode_error: StdMutex::new(None),
            exit_requested: AtomicBool::new(false),
            memory_path: StdMutex::new(None),
            editor_exit_code: AtomicI32::new(0),
            memory_error: StdMutex::new(None),
            cost_nano_usd: AtomicU64::new(0),
            cost_tokens: AtomicU64::new(0),
            cost_snapshot: StdMutex::new(None),
            mcp_servers: StdMutex::new(Vec::new()),
            skills: StdMutex::new(Vec::new()),
            hooks_list: StdMutex::new(Vec::new()),
            active_goal: StdMutex::new(None),
            workspace_trusted: AtomicBool::new(true),
            hooks_restricted: AtomicBool::new(false),
            agents_list: StdMutex::new(Vec::new()),
            doctor_report: StdMutex::new(DoctorReport::default()),
            status_snapshot: StdMutex::new(StatusSnapshot::default()),
            config_editor_error: StdMutex::new(None),
            permissions_editor_error: StdMutex::new(None),
            available_models: StdMutex::new(Vec::new()),
            files_in_context: StdMutex::new(Vec::new()),
            model_listings: StdMutex::new(Vec::new()),
            slash_command_transcript: StdMutex::new(Vec::new()),
            mod_describe_inputs: StdMutex::new(Vec::new()),
            mod_describe_rewrite: StdMutex::new(None),
            background_notices: StdMutex::new(Vec::new()),
        }
    }

    /// Replace one command's menu fields when the catalog asks the engine to describe it.
    pub fn set_mod_describe_rewrite(
        &self,
        name: &str,
        description: &str,
        argument_hint: Option<&str>,
        hidden: bool,
    ) {
        *self.mod_describe_rewrite.lock().unwrap() = Some((
            name.to_owned(),
            description.to_owned(),
            argument_hint.map(str::to_owned),
            hidden,
        ));
    }

    /// Inputs received from a slash catalog projection.
    pub fn mod_describe_inputs(&self) -> Vec<serde_json::Value> {
        self.mod_describe_inputs.lock().unwrap().clone()
    }

    /// Make the next `clear_session` call return `ActionFailed(reason)`.
    pub fn set_clear_session_error(&self, reason: String) {
        *self.clear_error.lock().unwrap() = Some(reason);
    }
    /// True if `clear_session` was called at least once.
    pub fn was_clear_session_called(&self) -> bool {
        self.clear_calls.load(Ordering::SeqCst) > 0
    }

    /// Pre-load the `CompactionSummary` returned by `force_compact`.
    pub fn set_compact_summary(&self, s: CompactionSummary) {
        *self.compact_summary.lock().unwrap() = Some(s);
    }
    /// Make the next `force_compact` call return `ActionFailed(reason)`.
    pub fn set_compact_error(&self, reason: String) {
        *self.compact_error.lock().unwrap() = Some(reason);
    }
    /// The `custom_instructions` most recently forwarded to
    /// `force_compact_with_instructions`, or `None` if it was never called.
    pub fn last_compact_instructions(&self) -> Option<String> {
        self.compact_instructions.lock().unwrap().clone()
    }

    /// True if `request_exit` was called.
    pub fn was_exit_requested(&self) -> bool {
        self.exit_requested.load(Ordering::SeqCst)
    }

    /// Pre-load the path `open_memory_editor` reports.
    pub fn set_memory_path(&self, p: PathBuf) {
        *self.memory_path.lock().unwrap() = Some(p);
    }
    /// Pre-load the exit code `open_memory_editor` reports.
    pub fn set_editor_exit_code(&self, c: i32) {
        self.editor_exit_code.store(c, Ordering::SeqCst);
    }
    /// Make the next `open_memory_editor` call return `ActionFailed(reason)`.
    pub fn set_memory_editor_error(&self, reason: String) {
        *self.memory_error.lock().unwrap() = Some(reason);
    }

    /// Enable authoritative controls snapshots; successful control setters update them.
    pub fn set_conversation_controls(&self, controls: lingxi_core::host::ConversationControls) {
        *self.permission_mode.lock().unwrap() = Some(controls.permission.effective.clone());
        *self.conversation_controls.lock().unwrap() = Some(controls);
    }

    /// Number of `switch_model` calls so far.
    pub fn switch_model_call_count(&self) -> usize {
        self.switch_model_calls.load(Ordering::SeqCst)
    }
    /// Most-recent model passed to `switch_model`, or `None`.
    pub fn last_switched_model(&self) -> Option<String> {
        self.switch_model_last.lock().unwrap().clone()
    }
    /// Most-recent `(model, profile)` pair passed to `switch_model`, or `None`
    /// if it has not been called yet.
    pub fn last_switch(&self) -> Option<(String, Option<String>)> {
        let model = self.switch_model_last.lock().unwrap().clone()?;
        let profile = self.switch_model_last_profile.lock().unwrap().clone()?;
        Some((model, profile))
    }
    /// Make the next `switch_model` call return `ActionFailed(reason)`.
    pub fn set_switch_model_error(&self, reason: String) {
        *self.switch_model_error.lock().unwrap() = Some(reason);
    }
    /// The mock's current live permission-mode wire id.
    pub fn current_permission_mode(&self) -> Option<String> {
        self.permission_mode.lock().unwrap().clone()
    }
    /// The mock's current session-scoped fast-mode flag.
    pub fn current_fast_mode(&self) -> bool {
        self.fast_mode.load(Ordering::SeqCst)
    }
    /// Make the next `set_permission_mode` call return `ActionFailed(reason)`.
    pub fn set_permission_mode_error(&self, reason: String) {
        *self.permission_mode_error.lock().unwrap() = Some(reason);
    }
    /// Seed the session-owned dynamic-workflow gate.
    pub fn set_dynamic_workflows_gate(&self, enabled: bool, managed: bool) {
        self.dynamic_workflows_gate.set(enabled, managed);
    }

    pub fn set_workflow_size_guideline(&self, value: &str, managed: bool, is_default: bool) {
        assert!(
            self.workflow_size_guideline
                .set_with_source(value, managed, is_default),
            "test must use a valid workflow size guideline"
        );
    }
    /// Pre-load the full `CostSnapshot` returned by `snapshot_cost`. If set,
    /// the snapshot is returned verbatim (with `session_id` overwritten to
    /// the mock's stable id).
    pub fn set_cost_snapshot(&self, s: lingxi_core::host::CostSnapshot) {
        *self.cost_snapshot.lock().unwrap() = Some(s);
    }
    // M5-11 setters:
    /// Pre-load the MCP server list returned by `list_mcp_servers`.
    pub fn set_mcp_servers(&self, v: Vec<McpServerInfo>) {
        *self.mcp_servers.lock().unwrap() = v;
    }
    /// Pre-load the skill list returned by `list_skills`.
    pub fn set_skills(&self, v: Vec<SkillInfo>) {
        *self.skills.lock().unwrap() = v;
    }
    /// Pre-load the hooks list returned by `list_hooks`.
    pub fn set_hooks(&self, v: Vec<HookInfo>) {
        *self.hooks_list.lock().unwrap() = v;
    }
    /// Pre-load the active `/goal` state returned by `get_active_goal`.
    pub fn set_active_goal_snapshot(&self, goal: Option<ActiveGoalSnapshot>) {
        *self.active_goal.lock().unwrap() = goal;
    }
    /// Configure the `/goal` workspace-trust gate.
    pub fn set_workspace_trusted(&self, trusted: bool) {
        self.workspace_trusted.store(trusted, Ordering::SeqCst);
    }
    /// Configure the `/goal` hooks-restricted gate.
    pub fn set_hooks_restricted(&self, restricted: bool) {
        self.hooks_restricted.store(restricted, Ordering::SeqCst);
    }
    /// Pre-load the agents list returned by `list_agents`.
    pub fn set_agents(&self, v: Vec<AgentInfo>) {
        *self.agents_list.lock().unwrap() = v;
    }
    /// Pre-load the doctor report returned by `run_doctor_checks`.
    pub fn set_doctor_report(&self, r: DoctorReport) {
        *self.doctor_report.lock().unwrap() = r;
    }
    /// Pre-load the status snapshot returned by `get_status_snapshot`.
    pub fn set_status_snapshot(&self, s: StatusSnapshot) {
        *self.status_snapshot.lock().unwrap() = s;
    }
    /// Make the next `edit_config_file` call return `ActionFailed(reason)`.
    pub fn set_config_editor_error(&self, e: String) {
        *self.config_editor_error.lock().unwrap() = Some(e);
    }
    /// Make the next `edit_permissions_file` call return `ActionFailed(reason)`.
    pub fn set_permissions_editor_error(&self, e: String) {
        *self.permissions_editor_error.lock().unwrap() = Some(e);
    }
    /// Pre-load the list returned by `list_available_models`.
    pub fn set_available_models(&self, m: Vec<String>) {
        *self.available_models.lock().unwrap() = m;
    }
    /// Pre-load the listing `output_styles` answers with.
    pub fn set_output_style_listing(&self, listing: lingxi_core::host::OutputStyleListing) {
        *self.output_style_listing.lock().unwrap() = Some(listing);
    }
    /// Every name `set_output_style` was called with, in order.
    #[must_use]
    pub fn output_style_switches(&self) -> Vec<String> {
        self.output_style_switches.lock().unwrap().clone()
    }
    /// Pre-load the read-file-state cache keys returned by `files_in_context`.
    pub fn set_files_in_context(&self, files: Vec<PathBuf>) {
        *self.files_in_context.lock().unwrap() = files;
    }
    /// Pre-load the model listings returned by `list_model_listings`.
    pub fn set_model_listings(&self, listings: Vec<lingxi_core::host::ModelListing>) {
        *self.model_listings.lock().unwrap() = listings;
    }

    /// Transcript pairs supplied through [`OrchestratorHandle::append_slash_command_transcript`].
    pub fn slash_command_transcript(&self) -> Vec<(String, String)> {
        self.slash_command_transcript.lock().unwrap().clone()
    }

    /// Every `body` passed to `emit_background_system_notice` so far, in
    /// call order.
    pub fn background_notices(&self) -> Vec<String> {
        self.background_notices.lock().unwrap().clone()
    }
}

impl Default for MockOrchestratorHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OrchestratorHandle for MockOrchestratorHandle {
    async fn current_session_id(&self) -> SessionId {
        self.session_id
    }

    async fn mod_describe_command(&self, mut input: serde_json::Value) -> serde_json::Value {
        self.mod_describe_inputs.lock().unwrap().push(input.clone());
        if let Some((name, description, hint, hidden)) = &*self.mod_describe_rewrite.lock().unwrap()
        {
            if input.get("command").and_then(serde_json::Value::as_str) == Some(name.as_str()) {
                input["description"] = serde_json::Value::String(description.clone());
                input["isHidden"] = serde_json::Value::Bool(*hidden);
                if let Some(hint) = hint {
                    input["argumentHint"] = serde_json::Value::String(hint.clone());
                } else if let Some(object) = input.as_object_mut() {
                    object.remove("argumentHint");
                }
            }
        }
        input
    }

    async fn append_slash_command_transcript(
        &self,
        raw: &str,
        display: &str,
    ) -> Result<(), HandleError> {
        self.slash_command_transcript
            .lock()
            .unwrap()
            .push((raw.to_string(), display.to_string()));
        Ok(())
    }

    async fn clear_session(&self) -> Result<(), HandleError> {
        self.clear_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(reason) = self.clear_error.lock().unwrap().take() {
            return Err(HandleError::ActionFailed(reason));
        }
        Ok(())
    }

    async fn force_compact(&self) -> Result<CompactionSummary, HandleError> {
        if let Some(reason) = self.compact_error.lock().unwrap().take() {
            return Err(HandleError::ActionFailed(reason));
        }
        Ok(self
            .compact_summary
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default())
    }

    /// Records the instructions so tests can assert `/compact <focus>` args
    /// actually reach the handle (the trait default silently swallows them —
    /// without this override a regression back to plain `force_compact()`
    /// would pass every suite).
    async fn force_compact_with_instructions(
        &self,
        custom_instructions: &str,
    ) -> Result<CompactionSummary, HandleError> {
        *self.compact_instructions.lock().unwrap() = Some(custom_instructions.to_string());
        self.force_compact().await
    }

    async fn snapshot_cost(&self) -> lingxi_core::host::CostSnapshot {
        if let Some(s) = self.cost_snapshot.lock().unwrap().clone() {
            // Force the session id to match the mock's stable id for
            // consistency with other handle methods.
            return lingxi_core::host::CostSnapshot {
                session_id: self.session_id,
                ..s
            };
        }
        lingxi_core::host::CostSnapshot {
            session_id: self.session_id,
            total_nano_usd: self.cost_nano_usd.load(Ordering::SeqCst),
            total_tokens: self.cost_tokens.load(Ordering::SeqCst),
            ..lingxi_core::host::CostSnapshot::default()
        }
    }

    async fn get_active_goal(&self) -> Option<ActiveGoalSnapshot> {
        self.active_goal.lock().unwrap().clone()
    }

    async fn set_active_goal(&self, condition: &str) {
        *self.active_goal.lock().unwrap() = Some(ActiveGoalSnapshot {
            condition: condition.to_string(),
            set_at: std::time::SystemTime::now(),
            last_reason: None,
            iterations: 0,
            tokens_at_start: self.cost_tokens.load(Ordering::SeqCst),
        });
    }

    async fn clear_active_goal(&self) -> Option<ActiveGoalSnapshot> {
        self.active_goal.lock().unwrap().take()
    }

    async fn set_active_goal_last_reason(&self, reason: Option<String>) {
        if let Some(goal) = self.active_goal.lock().unwrap().as_mut() {
            goal.last_reason = reason;
        }
    }

    async fn workspace_trusted(&self) -> bool {
        self.workspace_trusted.load(Ordering::SeqCst)
    }

    async fn hooks_restricted(&self) -> bool {
        self.hooks_restricted.load(Ordering::SeqCst)
    }

    async fn switch_model(&self, model: &str, profile: Option<&str>) -> Result<(), HandleError> {
        self.switch_model_calls.fetch_add(1, Ordering::SeqCst);
        *self.switch_model_last.lock().unwrap() = Some(model.to_string());
        *self.switch_model_last_profile.lock().unwrap() = Some(profile.map(str::to_string));
        if let Some(reason) = self.switch_model_error.lock().unwrap().take() {
            return Err(HandleError::ActionFailed(reason));
        }
        if let Some(controls) = self.conversation_controls.lock().unwrap().as_mut() {
            controls.model_reference = lingxi_core::host::qualified_model_ref(model, profile);
        }
        Ok(())
    }

    async fn permission_mode(&self) -> Option<String> {
        self.permission_mode.lock().unwrap().clone()
    }

    async fn conversation_controls(&self) -> Option<lingxi_core::host::ConversationControls> {
        self.conversation_controls.lock().unwrap().clone()
    }

    async fn set_reasoning_selection(
        &self,
        selection: lingxi_core::host::ReasoningSelection,
    ) -> Result<(), HandleError> {
        if let Some(controls) = self.conversation_controls.lock().unwrap().as_mut() {
            controls.requested_reasoning_selection = selection.clone();
            controls.effective_reasoning_selection = selection;
        }
        Ok(())
    }

    async fn current_effort(&self) -> Option<String> {
        self.effort.lock().unwrap().clone()
    }

    async fn fast_mode(&self) -> bool {
        self.fast_mode.load(Ordering::SeqCst)
    }

    async fn set_fast_mode(&self, on: bool) -> Result<(), HandleError> {
        self.fast_mode.store(on, Ordering::SeqCst);
        Ok(())
    }

    async fn ultracode_enabled(&self) -> bool {
        self.ultracode_enabled.load(Ordering::Acquire)
    }
    async fn set_ultracode_enabled(&self, enabled: bool) -> Result<(), HandleError> {
        self.ultracode_enabled.store(enabled, Ordering::Release);
        Ok(())
    }

    async fn dynamic_workflows_enabled(&self) -> bool {
        self.dynamic_workflows_gate.enabled()
    }

    async fn dynamic_workflows_managed(&self) -> bool {
        self.dynamic_workflows_gate.managed()
    }

    async fn workflow_size_guideline(&self) -> String {
        self.workflow_size_guideline.value().to_string()
    }

    async fn workflow_size_guideline_managed(&self) -> bool {
        self.workflow_size_guideline.managed()
    }

    async fn workflow_size_guideline_state(
        &self,
    ) -> lingxi_core::host::session_flags::WorkflowSizeGuidelineSnapshot {
        self.workflow_size_guideline.snapshot()
    }

    async fn workflow_size_guideline_is_default(&self) -> bool {
        self.workflow_size_guideline.is_default()
    }

    async fn set_dynamic_workflows_enabled(
        &self,
        enabled: bool,
        managed: bool,
    ) -> Result<(), HandleError> {
        self.dynamic_workflows_gate.set(enabled, managed);
        Ok(())
    }

    async fn set_workflow_size_guideline(
        &self,
        value: String,
        managed: bool,
        is_default: bool,
    ) -> Result<(), HandleError> {
        if self
            .workflow_size_guideline
            .set_with_source(&value, managed, is_default)
        {
            Ok(())
        } else {
            Err(HandleError::ActionFailed(format!(
                "invalid workflowSizeGuideline: {value}"
            )))
        }
    }

    async fn output_styles(&self) -> Option<lingxi_core::host::OutputStyleListing> {
        self.output_style_listing.lock().unwrap().clone()
    }

    async fn set_output_style(&self, name: &str) -> Result<(), HandleError> {
        self.output_style_switches
            .lock()
            .unwrap()
            .push(name.to_string());
        Ok(())
    }

    async fn set_session_effort(
        &self,
        effort: lingxi_core::host::effort_table::SessionEffort,
    ) -> Result<(), HandleError> {
        *self.effort.lock().unwrap() = match &effort {
            lingxi_core::host::effort_table::SessionEffort::Level(serde_json::Value::String(
                value,
            )) => Some(value.clone()),
            _ => None,
        };
        *self.effort_session.lock().unwrap() = effort;
        Ok(())
    }
    async fn effort_command_snapshot(
        &self,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, HandleError> {
        let source = self
            .effort_command_source
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| HandleError::Unimplemented("effort_command_snapshot".into()))?;
        let model = self.status_snapshot.lock().unwrap().model.clone();
        source(&model, self.effort_session.lock().unwrap().clone())
    }
    async fn reasoning_default_settings_path(&self) -> Option<PathBuf> {
        self.reasoning_default_path.lock().unwrap().clone()
    }

    async fn set_permission_mode(&self, mode: &str) -> Result<(), HandleError> {
        if let Some(reason) = self.permission_mode_error.lock().unwrap().take() {
            return Err(HandleError::ActionFailed(reason));
        }
        *self.permission_mode.lock().unwrap() = Some(mode.to_string());
        if let Some(controls) = self.conversation_controls.lock().unwrap().as_mut() {
            controls.permission.requested = mode.to_string();
            controls.permission.effective = mode.to_string();
        }
        Ok(())
    }

    async fn request_exit(&self) {
        self.exit_requested.store(true, Ordering::SeqCst);
    }

    async fn current_should_exit(&self) -> bool {
        self.exit_requested.load(Ordering::SeqCst)
    }

    async fn open_memory_editor(&self) -> Result<MemoryEditorOutcome, HandleError> {
        if let Some(reason) = self.memory_error.lock().unwrap().take() {
            return Err(HandleError::ActionFailed(reason));
        }
        Ok(MemoryEditorOutcome {
            edited_path: self
                .memory_path
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| PathBuf::from("/dev/null/LINGXI.md")),
            exit_code: self.editor_exit_code.load(Ordering::SeqCst),
        })
    }

    // M5-11 additions:

    async fn list_mcp_servers(&self) -> Vec<McpServerInfo> {
        self.mcp_servers.lock().unwrap().clone()
    }

    async fn list_skills(&self) -> Vec<SkillInfo> {
        self.skills.lock().unwrap().clone()
    }

    async fn list_hooks(&self) -> Vec<HookInfo> {
        self.hooks_list.lock().unwrap().clone()
    }

    async fn list_agents(&self) -> Vec<AgentInfo> {
        self.agents_list.lock().unwrap().clone()
    }

    async fn run_doctor_checks(&self) -> DoctorReport {
        self.doctor_report.lock().unwrap().clone()
    }

    async fn get_status_snapshot(&self) -> StatusSnapshot {
        self.status_snapshot.lock().unwrap().clone()
    }

    async fn edit_config_file(&self) -> Result<MemoryEditorOutcome, HandleError> {
        if let Some(e) = self.config_editor_error.lock().unwrap().take() {
            return Err(HandleError::ActionFailed(e));
        }
        Ok(MemoryEditorOutcome {
            edited_path: PathBuf::from("/tmp/mock/config.json"),
            exit_code: 0,
        })
    }

    async fn edit_permissions_file(&self) -> Result<MemoryEditorOutcome, HandleError> {
        if let Some(e) = self.permissions_editor_error.lock().unwrap().take() {
            return Err(HandleError::ActionFailed(e));
        }
        Ok(MemoryEditorOutcome {
            edited_path: PathBuf::from("/tmp/mock/permissions.json"),
            exit_code: 0,
        })
    }

    async fn list_available_models(&self) -> Vec<String> {
        self.available_models.lock().unwrap().clone()
    }

    async fn emit_background_system_notice(&self, body: &str) {
        self.background_notices
            .lock()
            .unwrap()
            .push(body.to_string());
    }

    async fn list_model_listings(&self) -> Vec<lingxi_core::host::ModelListing> {
        self.model_listings.lock().unwrap().clone()
    }

    async fn files_in_context(&self) -> Vec<PathBuf> {
        self.files_in_context.lock().unwrap().clone()
    }

    /// Deterministic fork outcome so wired-success tests can assert a real
    /// render. Note: `/fork`'s handler gates on `conversation_transcript`
    /// (default empty here) BEFORE calling this, so exercising this override
    /// end-to-end needs a handle that also reports an assistant turn.
    async fn fork_conversation(
        &self,
        _directive: &str,
    ) -> Result<lingxi_core::host::ForkOutcome, HandleError> {
        Ok(lingxi_core::host::ForkOutcome {
            name: "mock-fork".to_string(),
            agent_id: "mock-agent-abcd".to_string(),
        })
    }

    /// Deterministic background-session-copy line so the default (agent-view
    /// enabled) `/fork` (`ForkBackgroundHandler`/`vAd`) renders a real result in
    /// tests. The composition root owns the exact text in production; here we
    /// return a fixed line regardless of the (optional) `prompt`.
    async fn fork_to_background_session(&self, _prompt: &str) -> Result<String, HandleError> {
        Ok("Copied conversation into a new background session (mock-bg-abcd).".to_string())
    }

    async fn background_conversation(
        &self,
        _snapshot: lingxi_core::host::BackgroundingSnapshot,
    ) -> Result<String, HandleError> {
        Ok("Moved conversation into a background session (mock-bg-abcd).".to_string())
    }

    /// Deterministic recap text so wired-success tests can assert real output.
    /// (`/recap`'s handler gates on a qualifying transcript turn before calling.)
    async fn generate_recap(&self) -> Result<lingxi_core::host::RecapOutcome, HandleError> {
        Ok(lingxi_core::host::RecapOutcome::Text(
            "mock recap".to_string(),
        ))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------- MockApiClient (Task 6) --------

    #[tokio::test]
    async fn mock_returns_responses_in_order() {
        let r1 = mock_message_response(
            vec![LlmContentBlock::Text {
                text: "one".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        );
        let r2 = mock_message_response(
            vec![LlmContentBlock::Text {
                text: "two".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        );
        let mock = MockApiClient::new(vec![r1, r2]);
        let resp1 = mock
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new("m", None, None, vec![], vec![]),
            ))
            .await
            .expect("first");
        let resp2 = mock
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new("m", None, None, vec![], vec![]),
            ))
            .await
            .expect("second");
        let LlmContentBlock::Text {
            text: first_text, ..
        } = &resp1.content[0]
        else {
            panic!("expected text block");
        };
        let LlmContentBlock::Text {
            text: second_text, ..
        } = &resp2.content[0]
        else {
            panic!("expected text block");
        };
        assert_eq!(first_text, "one");
        assert_eq!(second_text, "two");
        assert_eq!(mock.remaining().await, 0);
    }

    #[tokio::test]
    async fn mock_captures_msgs_per_call() {
        let r = mock_message_response(vec![], Some("end_turn"));
        let mock = MockApiClient::new(vec![r]);
        let msgs = vec![];
        mock.messages_create(crate::OrchestratorApiRequest::Main(
            llm_runtime::MessagesCreateRequest::new("m", None, None, msgs, vec![]),
        ))
        .await
        .expect("call");
        assert_eq!(mock.captured_msgs().await.len(), 1);
    }

    #[tokio::test]
    async fn mock_exhaustion_returns_server_error() {
        let mock = MockApiClient::new(vec![]);
        let err = mock
            .messages_create(crate::OrchestratorApiRequest::Main(
                llm_runtime::MessagesCreateRequest::new("m", None, None, vec![], vec![]),
            ))
            .await
            .expect_err("exhausted");
        assert!(format!("{err}").contains("mock script exhausted"));
    }

    // -------- MockOutputStream (Task 7) --------

    #[tokio::test]
    async fn mock_output_stream_captures_text() {
        let m = MockOutputStream::new();
        m.emit_text("hello", None).await;
        m.emit_text("world", None).await;
        let texts = m.text_events().await;
        assert_eq!(texts, vec!["hello".to_string(), "world".to_string()]);
    }

    #[tokio::test]
    async fn mock_output_stream_captures_tool_lifecycle() {
        let m = MockOutputStream::new();
        let id = lingxi_core::types::ToolUseId::new();
        let input = serde_json::json!({"file_path": "/tmp/x"});
        let result = serde_json::json!({"content": "ok"});
        m.emit_tool_call(&id, "Read", &input, None).await;
        m.emit_tool_heartbeat(&id, "Read", 1_250).await;
        m.emit_tool_result(&id, "Read", "ok", &result, None).await;
        let snap = m.snapshot().await;
        assert_eq!(snap.len(), 3);
        assert!(matches!(&snap[0], OutputEvent::ToolCall { id: gid, .. } if *gid == id));
        assert!(matches!(
            &snap[1],
            OutputEvent::ToolHeartbeat {
                id: gid,
                elapsed_ms: 1_250,
                ..
            } if *gid == id
        ));
        assert!(matches!(&snap[2], OutputEvent::ToolResult { id: gid, .. } if *gid == id));
    }

    #[tokio::test]
    async fn mock_output_stream_captures_end_turn() {
        let m = MockOutputStream::new();
        // SessionId::default() mints a fresh v4 UUID, so we can't compare two
        // `CostSnapshot::default()` instances structurally. Bind a single
        // cost value and check the captured Clone matches that instance.
        let cost = CostSnapshot::default();
        m.emit_end_turn("end_turn", &cost).await;
        let snap = m.snapshot().await;
        assert_eq!(snap.len(), 1);
        match &snap[0] {
            OutputEvent::EndTurn {
                stop_reason,
                cost: c,
            } => {
                assert_eq!(stop_reason, "end_turn");
                assert_eq!(c, &cost);
                assert_eq!(c.total_nano_usd, 0);
                assert_eq!(c.total_tokens, 0);
            }
            _ => panic!("expected EndTurn"),
        }
    }

    // -------- NoOp hooks + permission (Task 8) --------

    #[tokio::test]
    async fn noop_hook_executor_returns_empty_aggregate() {
        let h = noop_hook_executor();
        let event = hooks::events::HookEvent::PreToolUse {
            tool_name: "Read".into(),
            tool_input: serde_json::json!({}),
            tool_use_id: lingxi_core::types::ToolUseId::new(),
        };
        let ctx = hooks::registry::HookContext::default();
        let agg = h.execute(event, ctx).await;
        assert!(agg.decision.is_none());
        assert!(agg.modified_input.is_none());
        assert!(agg.system_messages.is_empty());
    }

    #[tokio::test]
    async fn noop_permission_gate_always_allows() {
        let g = NoOpPermissionGate;
        let v = serde_json::json!({});
        assert_eq!(g.check("Read", &v).await, PermissionDecision::Allow);
        assert_eq!(g.check("Bash", &v).await, PermissionDecision::Allow);
    }

    // M6-08 Task 4: MockOutputStream must capture CompactionCompleted.
    #[tokio::test]
    async fn mock_output_records_compaction_completed() {
        let m = MockOutputStream::new();
        m.emit_compaction_completed(42, 7, 1234, "Summary:\nkept context")
            .await;
        let events = m.snapshot().await;
        let last = events.last().expect("at least one event");
        assert!(
            matches!(
                last,
                OutputEvent::CompactionCompleted {
                    messages_before: 42,
                    messages_after: 7,
                    bytes_saved: 1234,
                    summary
                } if summary == "Summary:\nkept context"
            ),
            "got: {last:?}"
        );
    }
}
