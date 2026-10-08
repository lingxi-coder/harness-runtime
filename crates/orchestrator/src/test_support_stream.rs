//! Test fixtures for the streaming path.
//!
//! - [`MockStreamingApiClient`] — implements [`crate::conversation::StreamingApiClient`]
//!   over a per-turn `Vec<HistoryEvent>` script.
//! - [`scripted!`] — declarative macro for assembling event sequences
//!   with the high-level vocabulary `text`, `tool_use`, `end_turn`,
//!   `tool_use_stop`, etc.
//! - [`MockToolDispatchClock`] — records the wall-clock instant each
//!   tool dispatch begins, for the mid-stream dispatch test.
//!
//! Mirroring `test_support`, this module is compiled unconditionally
//! (not feature-gated) so integration tests can use the fixtures
//! without needing `--features test-support`. Production builds drop
//! the unused symbols at link-time.
#![forbid(unsafe_code)]

use crate::conversation::StreamingApiClient;
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use lingxi_core::types::{ConversationMessage, ToolUseId};
use llm_runtime::{
    ContentBlock as LlmContentBlock, ExecutionUsage as Usage, HistoryContentDelta, HistoryEvent,
    HistoryMessageDelta, HistoryResponse, LlmError,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

/// Captured arguments of one `StreamingApiClient::stream` call.
#[derive(Debug, Clone)]
pub struct CapturedStreamCall {
    /// Model name as requested by the orchestrator.
    pub model: String,
    /// Provider profile name threaded from `SessionState::model_profile`
    /// (`None` when no profile is active on the session).
    pub profile: Option<String>,
    /// Per-request override supplied by a `turn.step` hook, if any.
    pub effort_override: Option<String>,
    /// Assembled system prompt (`None` if omitted).
    pub system: Option<lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
    /// Conversation history snapshot at the time of the call.
    pub messages: Vec<ConversationMessage>,
    /// Wire tool definitions advertised on this call (`build_wire_tools`).
    pub tools: Vec<Value>,
    /// Sanitized Native query source used for provider cache policy.
    pub query_source: String,
    pub skip_global_cache_for_system_prompt: bool,
}

/// Mock streaming client. Yields the next per-turn script of events each
/// time `stream` is called. If the queue is exhausted, returns
/// `LlmError::Transport { message: "streaming script exhausted" }`.
pub struct MockStreamingApiClient {
    turns: Mutex<std::collections::VecDeque<Vec<Result<HistoryEvent, LlmError>>>>,
    captured: Arc<Mutex<Vec<CapturedStreamCall>>>,
    /// One-shot connect-phase error returned by the NEXT `stream()` call (then
    /// cleared), so a test can mimic the real adapter returning `Err` from
    /// `stream()` itself — e.g. a connect-phase 413/`ContextOverflow`, which the
    /// real `drive_stream` surfaces by returning `Err` on status >= 400 rather
    /// than as a stream event. `None` ⇒ normal scripted behavior.
    open_error: Mutex<Option<LlmError>>,
}

impl MockStreamingApiClient {
    /// Construct an empty (always-exhausted) mock.
    #[must_use]
    pub fn empty() -> Self {
        Self::with_turns(Vec::new())
    }

    /// Construct from a Vec where each inner Vec is the scripted event
    /// sequence for one turn.
    #[must_use]
    pub fn with_turns(turns: Vec<Vec<HistoryEvent>>) -> Self {
        let mapped: Vec<Vec<Result<HistoryEvent, LlmError>>> = turns
            .into_iter()
            .map(|t| t.into_iter().map(Ok).collect())
            .collect();
        Self {
            turns: Mutex::new(mapped.into()),
            captured: Arc::new(Mutex::new(Vec::new())),
            open_error: Mutex::new(None),
        }
    }

    /// Construct from already-fallible turns (used to inject an `Err`
    /// mid-stream for the error-propagation test).
    #[must_use]
    pub fn with_fallible_turns(turns: Vec<Vec<Result<HistoryEvent, LlmError>>>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            captured: Arc::new(Mutex::new(Vec::new())),
            open_error: Mutex::new(None),
        }
    }

    /// Construct a mock whose FIRST `stream()` call returns `Err(err)` (the
    /// connect-phase failure path, mimicking the real adapter's `drive_stream`
    /// returning `Err` on a >= 400 connect response — e.g. a 413/PTL). The
    /// remaining `turns` script serves any subsequent calls.
    #[must_use]
    pub fn with_open_error(err: LlmError, turns: Vec<Vec<HistoryEvent>>) -> Self {
        let mut me = Self::with_turns(turns);
        me.open_error = Mutex::new(Some(err));
        me
    }

    /// Snapshot the captured `stream` call args (one entry per call).
    pub async fn captured_calls(&self) -> Vec<CapturedStreamCall> {
        self.captured.lock().await.clone()
    }
}

#[async_trait]
impl StreamingApiClient for MockStreamingApiClient {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<Value>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        if request_dispatch_admission
            .as_ref()
            .is_some_and(|admission| !admission.is_admitted())
        {
            return Err(LlmError::RequestDispatchRejected {
                prior_dispatch: false,
            });
        }
        self.captured.lock().await.push(CapturedStreamCall {
            model: model.to_string(),
            profile: profile.map(str::to_string),
            effort_override: None,
            system: system.cloned(),
            messages,
            tools,
            query_source: query_source.to_string(),
            skip_global_cache_for_system_prompt,
        });
        // One-shot connect-phase error (see `with_open_error`).
        if let Some(e) = self.open_error.lock().await.take() {
            return Err(e);
        }
        let mut queue = self.turns.lock().await;
        let next = queue.pop_front().ok_or_else(|| LlmError::Transport {
            message: "streaming script exhausted".into(),
        })?;
        let s = stream::iter(next).boxed();
        Ok(s)
    }

    async fn stream_with_effort_override(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<Value>,
        effort: Option<&str>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError> {
        let stream = self
            .stream(
                model,
                profile,
                system,
                messages,
                tools,
                query_source,
                skip_global_cache_for_system_prompt,
                request_dispatch_admission,
            )
            .await?;
        if let Some(last) = self.captured.lock().await.last_mut() {
            last.effort_override = effort.map(str::to_owned);
        }
        Ok(stream)
    }
}

/// Records wall-clock instants when tool dispatches begin. Used by the
/// mid-stream test to assert dispatch fires BEFORE `message_stop`.
#[derive(Debug, Default, Clone)]
pub struct MockToolDispatchClock {
    /// Each entry is `(tool name, instant the dispatch began)`.
    pub dispatch_instants: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl MockToolDispatchClock {
    /// Record that a tool dispatch began at this instant.
    pub async fn record(&self, tool: &str) {
        self.dispatch_instants
            .lock()
            .await
            .push((tool.to_string(), Instant::now()));
    }
    /// Snapshot the dispatch instants.
    pub async fn snapshot(&self) -> Vec<(String, Instant)> {
        self.dispatch_instants.lock().await.clone()
    }
}

// ─── scripted! macro helpers ───────────────────────────────────────────

fn default_usage() -> Usage {
    Usage::default()
}

/// `message_start` event with the given id + model.
#[must_use]
pub fn message_start(id: &str, model: &str) -> HistoryEvent {
    HistoryEvent::MessageStart {
        response: Box::new(HistoryResponse {
            id: id.to_string(),
            model: model.to_string(),
            content: Vec::new(),
            stop_reason: None,
            stop_details: None,
            usage: default_usage(),
            cost: None,
            provider_metadata: serde_json::Value::Null,
        }),
    }
}

/// `message_start` event with an explicit `usage` snapshot.
///
/// On the real Anthropic wire, `message_start.usage` carries the
/// `input_tokens` + cache counts; `output_tokens` is `0` here and
/// arrives later in `message_delta.usage`.
#[must_use]
pub fn message_start_with_usage(id: &str, model: &str, usage: Usage) -> HistoryEvent {
    HistoryEvent::MessageStart {
        response: Box::new(HistoryResponse {
            id: id.to_string(),
            model: model.to_string(),
            content: Vec::new(),
            stop_reason: None,
            stop_details: None,
            usage,
            cost: None,
            provider_metadata: serde_json::Value::Null,
        }),
    }
}

/// `content_block_start` for a `text` block at `index`.
#[must_use]
pub fn content_block_start_text(index: u32) -> HistoryEvent {
    HistoryEvent::ContentBlockStart {
        index,
        content_block: LlmContentBlock::Text {
            text: String::new(),
            cache_control: None, citations: None,
        },
    }
}

/// `content_block_start` for a `tool_use` block at `index`. The `id`
/// parameter accepts a raw `ToolUseId` so test scripts can correlate
/// dispatches with the eventual `ToolResult`.
#[must_use]
pub fn content_block_start_tool_use(index: u32, id: ToolUseId, name: &str) -> HistoryEvent {
    HistoryEvent::ContentBlockStart {
        index,
        content_block: LlmContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input: Value::Object(serde_json::Map::new()),
        },
    }
}

/// `content_block_delta { delta: TextDelta { text } }`.
#[must_use]
pub fn text_delta(index: u32, text: &str) -> HistoryEvent {
    HistoryEvent::ContentBlockDelta {
        index,
        delta: HistoryContentDelta::TextDelta {
            text: text.to_string(),
        },
    }
}

/// `content_block_delta { delta: InputJsonDelta { partial_json } }`.
#[must_use]
pub fn input_json_delta(index: u32, partial: &str) -> HistoryEvent {
    HistoryEvent::ContentBlockDelta {
        index,
        delta: HistoryContentDelta::InputJsonDelta {
            partial_json: partial.to_string(),
        },
    }
}

/// `content_block_stop { index }`.
#[must_use]
pub fn content_block_stop(index: u32) -> HistoryEvent {
    HistoryEvent::ContentBlockStop { index }
}

/// `message_delta { delta: { stop_reason } }`.
#[must_use]
pub fn message_delta_stop(stop_reason: &str) -> HistoryEvent {
    HistoryEvent::MessageDelta {
        delta: HistoryMessageDelta {
            stop_reason: Some(stop_reason.to_string()),
            stop_details: None,
        },
        usage: None,
    }
}

/// `content_block_start` for a `thinking` (Reasoning) block at `index`. (§0.7
/// "light up thinking/usage" test vocabulary.)
#[must_use]
pub fn content_block_start_thinking(index: u32) -> HistoryEvent {
    HistoryEvent::ContentBlockStart {
        index,
        content_block: LlmContentBlock::Reasoning {
            text: String::new(),
            signature: None,
        },
    }
}

/// `content_block_delta { delta: ThinkingDelta { thinking } }`. (§0.7
/// "light up thinking/usage" test vocabulary.)
#[must_use]
pub fn thinking_delta(index: u32, thinking: &str) -> HistoryEvent {
    HistoryEvent::ContentBlockDelta {
        index,
        delta: HistoryContentDelta::ThinkingDelta {
            thinking: thinking.to_string(),
        },
    }
}

/// `message_delta` carrying a `stop_reason` AND a final `usage` snapshot.
/// (§0.7 "light up thinking/usage" test vocabulary.)
#[must_use]
pub fn message_delta_stop_with_usage(stop_reason: &str, usage: Usage) -> HistoryEvent {
    HistoryEvent::MessageDelta {
        delta: HistoryMessageDelta {
            stop_reason: Some(stop_reason.to_string()),
            stop_details: None,
        },
        usage: Some(usage),
    }
}

/// `message_stop`.
#[must_use]
pub fn message_stop() -> HistoryEvent {
    HistoryEvent::MessageStop
}

/// No-op keepalive — `HistoryEvent` has no `Ping` variant so this just returns
/// a `MessageStop` as a harmless stand-in for any keepalive-like event in
/// tests that use it as a filler. Most callers should use `message_stop()`
/// directly instead.
///
/// NOTE: this function exists for back-compat with callers that used the
/// old `api_client` `StreamEvent::Ping`-based `ping()` helper. It emits a
/// `MessageStop` — tests that used `ping()` as a mid-stream no-op should
/// be updated to remove the call or replace it with a real event.
#[must_use]
pub fn ping() -> HistoryEvent {
    // HistoryEvent has no Ping; emit a harmless ContentBlockStop at a dummy
    // index (u32::MAX) that the accumulator will treat as "skipped"
    // because no block was started at that index.  Not ideal but preserves
    // compilation for legacy callers — migrate them to remove ping() calls.
    HistoryEvent::ContentBlockStop { index: u32::MAX }
}

/// Declarative macro for assembling an event sequence. Pass any
/// expression that evaluates to a `HistoryEvent`. Example:
/// ```ignore
/// let s = scripted![
///     message_start("msg_1", "claude-opus-4-7"),
///     content_block_start_text(0),
///     text_delta(0, "hello"),
///     content_block_stop(0),
///     message_delta_stop("end_turn"),
///     message_stop(),
/// ];
/// ```
#[macro_export]
macro_rules! scripted {
    [$($event:expr),* $(,)?] => {
        vec![$($event),*]
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_yields_first_turn_then_exhausts() {
        let mock = MockStreamingApiClient::with_turns(vec![scripted![
            message_start("m1", "claude-opus-4-7"),
            message_stop(),
        ]]);
        let s = mock
            .stream("claude-opus-4-7", None, None, Vec::new(), Vec::new(), "sdk", false, None)
            .await
            .expect("first turn");
        let collected: Vec<_> = s.collect().await;
        assert_eq!(collected.len(), 2);
        assert!(matches!(
            collected[0],
            Ok(HistoryEvent::MessageStart { .. })
        ));
        assert!(matches!(collected[1], Ok(HistoryEvent::MessageStop)));

        let result = mock
            .stream("claude-opus-4-7", None, None, Vec::new(), Vec::new(), "sdk", false, None)
            .await;
        // `Result::expect_err` requires `Ok` to be `Debug`; `BoxStream`
        // is not. Match on the result instead.
        match result {
            Ok(_) => panic!("expected exhaustion error"),
            Err(e) => assert!(matches!(e, LlmError::Transport { .. })),
        }
    }

    #[tokio::test]
    async fn captured_calls_record_model_and_system() {
        let mock = MockStreamingApiClient::with_turns(vec![scripted![message_stop()]]);
        let result = mock
            .stream(
                "claude-opus-4-7",
                None,
                Some(&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::custom_prompt(
                    lingxi_llm_client::providers::anthropic::system_prompt::PromptText::from_string("sys"),
                )),
                Vec::new(),
                Vec::new(),
                "sdk", false,
                None,
            )
            .await;
        // `Result::expect` requires Ok = `BoxStream` to be Debug;
        // it is not. Discriminate via `is_ok` instead.
        assert!(result.is_ok(), "call should succeed");
        let calls = mock.captured_calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model, "claude-opus-4-7");
        assert_eq!(
            calls[0].system.as_ref().map(|system| system.display_text()),
            Some("sys".to_owned())
        );
    }
}
