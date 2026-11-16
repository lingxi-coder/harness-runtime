//! Exact prompt/reply construction through real turn, cancellation and JSONL paths.
use async_trait::async_trait;
use lingxi_core::host::{FileSystem, NewDiagnosticsSource};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage};
use llm_runtime::{HistoryContentDelta, HistoryEvent};
use orchestrator::prompt::mid_turn_input::MidTurnInputSource;
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, OrchestratorApiClient, OrchestratorConfig, StreamingApiClient,
    TurnOutcome,
};
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MODEL: &str = "gpt-6.1-sol";

fn source(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).unwrap()
}

fn fixture(
    cwd: &Path,
) -> (
    ConversationOrchestrator,
    Arc<MockApiClient>,
    Arc<MockStreamingApiClient>,
) {
    let reply = "reply 中文😀e\u{301}";
    let units: Vec<_> = reply.encode_utf16().collect();
    let split = units
        .iter()
        .position(|unit| (0xd800..=0xdbff).contains(unit))
        .unwrap()
        + 1;
    let mut response = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: reply.into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    response.model = MODEL.into();
    let events = vec![
        message_start("exact-reply", MODEL),
        content_block_start_text(0),
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextJsUtf16Delta {
                text: String::from_utf16_lossy(&units[..split]),
                utf16_code_units: units[..split].to_vec(),
            },
        },
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextJsUtf16Delta {
                text: String::from_utf16_lossy(&units[split..]),
                utf16_code_units: units[split..].to_vec(),
            },
        },
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let api = Arc::new(MockApiClient::new(vec![response; 4]));
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![events; 4]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.into(),
            ..Default::default()
        },
        api.clone(),
        streaming.clone(),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_owned(),
    )
    .with_config_home(cwd.join("config"));
    (orch, api, streaming)
}

struct InputGate {
    first: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    queued: tokio::sync::Mutex<Option<Utf16JsonProjection>>,
}
impl InputGate {
    fn new() -> Self {
        Self {
            first: AtomicBool::new(true),
            entered: Default::default(),
            release: Default::default(),
            queued: Default::default(),
        }
    }
}
#[async_trait]
impl MidTurnInputSource for InputGate {
    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        self.queued.lock().await.take()
    }
}
#[async_trait]
impl NewDiagnosticsSource for InputGate {
    async fn take_new_diagnostics_block(&self) -> Option<Utf16JsonProjection> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        None
    }
}

async fn run(orch: &ConversationOrchestrator, streaming: bool, prompt: &Utf16JsonProjection) {
    if streaming {
        orch.run_turn_streaming(prompt).await.unwrap();
    } else {
        orch.run_turn(prompt).await.unwrap();
    }
}

fn exact_user_rows(history: &[ConversationMessage], units: &[u16]) -> Vec<ConversationMessage> {
    history
        .iter()
        .filter(|message| {
            matches!(message, ConversationMessage::User {content, ..}
            if content.iter().any(|block| match block {
                ContentBlock::Text {text, ..} => text.encode_utf16().eq(units.iter().copied()),
                ContentBlock::TextJsUtf16 {utf16_code_units, ..} => utf16_code_units == units,
                _ => false,
            }))
        })
        .cloned()
        .collect()
}

fn wire_input(history: Vec<ConversationMessage>, streaming: bool) -> Vec<String> {
    use lingxi_llm_client::codecs::{CodecContext, EncodeRequest, WireCodec};
    use lingxi_llm_client::protocol::JsonPayload;
    let history = llm_runtime::convert::normalize_messages_for_api(history);
    let messages = llm_runtime::convert::to_llm_messages(history).unwrap();
    let (request, exact) = llm_runtime::convert::history_input(
        MODEL,
        &messages,
        &[],
        &[],
        llm_runtime::ProtocolFamily::OpenAiResponses,
    )
    .unwrap();
    let profile = serde_json::from_value(serde_json::json!({
        "provider_id":"openai", "profile_name":"fixture", "auth":"none",
        "protocol":"open_ai_responses", "base_url":"https://api.openai.com/v1",
        "models":[{"display_model":MODEL, "request_model":MODEL, "billing_model":MODEL}]
    }))
    .unwrap();
    let mode = if streaming {
        lingxi_llm_client::RequestMode::Stream
    } else {
        lingxi_llm_client::RequestMode::Complete
    };
    let prepared = lingxi_llm_client::OpenAiResponsesCodec
        .encode_request(
            EncodeRequest::new(&request),
            &CodecContext::new(&profile, MODEL, mode),
        )
        .unwrap();
    let body = JsonPayload::parse(std::str::from_utf8(&prepared.body).unwrap()).unwrap();
    let overrides = lingxi_llm_client::exact_json::map_message_text_overrides(
        &request,
        llm_runtime::ProtocolFamily::OpenAiResponses,
        &body,
        &exact,
    )
    .unwrap();
    let encoded = lingxi_llm_client::exact_json::serialize_json_payload_with_overrides(
        &body,
        &overrides,
        lingxi_llm_client::exact_json::JsonEncoding::Serde,
    )
    .unwrap();
    let body = JsonPayload::parse(std::str::from_utf8(&encoded).unwrap()).unwrap();
    (0..body.array_len("/input").unwrap())
        .map(|index| {
            body.array_item_at("/input", index)
                .unwrap()
                .as_json_source()
                .to_owned()
        })
        .collect()
}

#[tokio::test]
async fn cancellation_then_continue_keeps_exact_prompt_and_hot_cold_wire_prefix() {
    for streaming in [false, true] {
        for cold in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let units = vec![0x61, 0xd800, 0x62, 0xdc00, 0xd83d, 0xde00];
            let prompt = source(units.clone());
            let gate = Arc::new(InputGate::new());
            let (orch, api, streamed) = fixture(root.path());
            let sid = orch.session().lock().await.session_id;
            let path = root.path().join(format!("{}.jsonl", sid.as_uuid()));
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_owned(),
            ));
            let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs));
            let orch = ConversationOrchestrator::into_shared(
                orch.with_jsonl_writer(writer.clone())
                    .with_new_diagnostics_source(gate.clone()),
            );
            let cancel = CancellationToken::new();
            let mut running = Box::pin(async {
                if streaming {
                    orch.run_turn_streaming_with_cancel(&prompt, cancel.clone())
                        .await
                } else {
                    orch.run_turn_with_cancel(&prompt, cancel.clone()).await
                }
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::select! { () = gate.entered.notified() => {}, result = &mut running => panic!("finished before gate: {result:?}") }
            }).await.unwrap();
            cancel.cancel();
            gate.release.notify_one();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), &mut running)
                    .await
                    .unwrap()
                    .unwrap(),
                TurnOutcome::Cancelled
            );
            drop(running);
            assert!(api.captured_msgs().await.is_empty());
            assert!(streamed.captured_calls().await.is_empty());
            let before = orch.snapshot_history().await;
            assert_eq!(exact_user_rows(&before, &units).len(), 1);
            writer.flush_queue_operations().await.unwrap();
            let active = if cold {
                let rows = session::jsonl::route_lines(&std::fs::read_to_string(&path).unwrap());
                assert_eq!(rows.malformed_line_count, 0);
                let restored = orchestrator::resume::state_from_messages(
                    sid.as_uuid(),
                    &rows.messages_in_order,
                );
                let (next, _, _) = fixture(root.path());
                let next = ConversationOrchestrator::into_shared(next.with_session_id(sid));
                *next.session().lock().await = restored;
                next
            } else {
                orch
            };
            let restored_rows = exact_user_rows(&active.snapshot_history().await, &units);
            assert_eq!(restored_rows.len(), 1);
            assert_eq!(
                wire_input(exact_user_rows(&before, &units), streaming),
                wire_input(restored_rows, streaming)
            );
            run(&active, streaming, &"continue 中文😀".into()).await;
            let after = active.snapshot_history().await;
            assert_eq!(exact_user_rows(&after, &units).len(), 1);
            assert!(after.iter().any(|message| matches!(
                message,
                ConversationMessage::Assistant { .. }
            ) && message.text_content()
                == "reply 中文😀e\u{301}"));
        }
    }
}

#[tokio::test]
async fn exact_user_input_appended_during_running_turn_keeps_order_and_previous_wire_prefix() {
    for streaming in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let initial = "initial 中文😀";
        let units = vec![0x78, 0xd801, 0x79, 0xdc01];
        let gate = Arc::new(InputGate::new());
        let (orch, api, streamed) = fixture(root.path());
        let orch = ConversationOrchestrator::into_shared(
            orch.with_new_diagnostics_source(gate.clone())
                .with_mid_turn_input(gate.clone()),
        );
        let mut running = Box::pin(async {
            if streaming {
                orch.run_turn_streaming(initial).await
            } else {
                orch.run_turn(initial).await
            }
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::select! { () = gate.entered.notified() => {}, result = &mut running => panic!("finished before gate: {result:?}") }
        }).await.unwrap();
        *gate.queued.lock().await = Some(source(units.clone()));
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(10), &mut running)
            .await
            .unwrap()
            .unwrap();
        drop(running);
        run(&orch, streaming, &"continue".into()).await;
        let requests = if streaming {
            streamed
                .captured_calls()
                .await
                .into_iter()
                .map(|call| call.messages)
                .collect()
        } else {
            api.captured_msgs().await
        };
        let first = wire_input(requests[0].clone(), streaming);
        let last = wire_input(requests.last().unwrap().clone(), streaming);
        assert_eq!(&last[..first.len()], first);
        let history = orch.snapshot_history().await;
        let original = history
            .iter()
            .position(|message| message.text_content() == initial)
            .unwrap();
        let appended: Vec<_> = history.iter().filter(|message| {
            matches!(message, ConversationMessage::User {content, ..} if content.iter().any(|block| {
                matches!(block, ContentBlock::TextJsUtf16 {utf16_code_units, ..}
                    if utf16_code_units.windows(units.len()).any(|part| part == units))
            }))
        }).collect();
        assert_eq!(appended.len(), 1);
        assert!(
            history
                .iter()
                .position(|message| message.id() == appended[0].id())
                .unwrap()
                > original
        );
    }
}

struct PausedResponse {
    api: Arc<MockApiClient>,
    streaming: Arc<MockStreamingApiClient>,
    first: AtomicBool,
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl OrchestratorApiClient for PausedResponse {
    async fn messages_create(
        &self,
        request: orchestrator::OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        let response = self.api.messages_create(request).await?;
        if self.first.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(response)
    }
}

#[async_trait]
impl StreamingApiClient for PausedResponse {
    async fn stream(
        &self,
        model: &str,
        profile: Option<&str>,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        messages: Vec<ConversationMessage>,
        tools: Vec<llm_runtime::ToolDeclaration>,
        query_source: &str,
        skip_global_cache_for_system_prompt: bool,
        request_dispatch_admission: Option<llm_runtime::RequestDispatchAdmission>,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<HistoryEvent, llm_runtime::LlmError>>,
        llm_runtime::LlmError,
    > {
        use futures::StreamExt;
        let events = self
            .streaming
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
        if !self.first.swap(false, Ordering::SeqCst) {
            return Ok(events);
        }
        let entered = self.entered.clone();
        // The third event ends with a high surrogate. Signal only after it has
        // reached the host; leave the remaining half and terminal unreceived.
        Ok(events
            .take(3)
            .chain(futures::stream::once(async move {
                entered.notify_one();
                std::future::pending::<Result<HistoryEvent, llm_runtime::LlmError>>().await
            }))
            .boxed())
    }
}

#[tokio::test]
async fn cancellation_after_response_data_resumes_without_reusing_partial_text_builder() {
    for streaming in [false, true] {
        for cold in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (_, api, streamed) = fixture(root.path());
            let entered = Arc::new(tokio::sync::Notify::new());
            let paused = Arc::new(PausedResponse {
                api,
                streaming: streamed,
                first: AtomicBool::new(true),
                entered: entered.clone(),
            });
            let output = Arc::new(MockOutputStream::new());
            let orch = ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: MODEL.into(),
                    ..Default::default()
                },
                paused.clone(),
                paused,
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_owned(),
            )
            .with_config_home(root.path().join("config"));
            let sid = orch.session().lock().await.session_id;
            let path = root.path().join(format!("{}.jsonl", sid.as_uuid()));
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_owned(),
            ));
            let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs));
            let orch =
                ConversationOrchestrator::into_shared(orch.with_jsonl_writer(writer.clone()));
            let units = vec![0x61, 0xd800, 0x62];
            let prompt = source(units.clone());
            let cancel = CancellationToken::new();
            let mut running = Box::pin(async {
                if streaming {
                    orch.run_turn_streaming_with_cancel(&prompt, cancel.clone())
                        .await
                } else {
                    orch.run_turn_with_cancel(&prompt, cancel.clone()).await
                }
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::select! { () = entered.notified() => {}, result = &mut running => panic!("finished before response pause: {result:?}") }
            }).await.unwrap();
            if streaming {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if output.text_events().await.iter().any(|text| text.starts_with("reply ") && text.ends_with('�')) { break; }
                        tokio::select! {
                            result = &mut running => panic!("turn ended before visible partial text: {result:?}"),
                            () = tokio::task::yield_now() => {},
                        }
                    }
                }).await.unwrap();
            }
            cancel.cancel();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(10), &mut running)
                    .await
                    .unwrap()
                    .unwrap(),
                TurnOutcome::Cancelled
            );
            drop(running);
            let before = orch.snapshot_history().await;
            // Native user cancellation commits completed blocks and its
            // interruption row. Unlike a transport close, it does not commit
            // an open text block. Retain this policy while checking that the
            // next attempt cannot inherit the dropped builder's high surrogate.
            assert!(!before.iter().any(|message| matches!(
                message,
                ConversationMessage::Assistant { .. }
            ) && message
                .text_content()
                .starts_with("reply ")));
            writer.flush_queue_operations().await.unwrap();
            let active = if cold {
                let rows = session::jsonl::route_lines(&std::fs::read_to_string(&path).unwrap());
                assert_eq!(rows.malformed_line_count, 0);
                let restored = orchestrator::resume::state_from_messages(
                    sid.as_uuid(),
                    &rows.messages_in_order,
                );
                let (next, _, _) = fixture(root.path());
                let next = ConversationOrchestrator::into_shared(next.with_session_id(sid));
                *next.session().lock().await = restored;
                next
            } else {
                orch
            };
            assert_eq!(
                wire_input(exact_user_rows(&before, &units), streaming),
                wire_input(
                    exact_user_rows(&active.snapshot_history().await, &units),
                    streaming
                )
            );
            run(&active, streaming, &"continue".into()).await;
            let after = active.snapshot_history().await;
            assert_eq!(exact_user_rows(&after, &units).len(), 1);
            assert_eq!(
                after
                    .iter()
                    .rev()
                    .find(|message| matches!(message, ConversationMessage::Assistant { .. }))
                    .unwrap()
                    .text_content(),
                "reply 中文😀e\u{301}"
            );
        }
    }
}
