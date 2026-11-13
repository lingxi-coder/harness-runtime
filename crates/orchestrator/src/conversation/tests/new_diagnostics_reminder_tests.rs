use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

struct MockDiag(Option<lingxi_core::types::utf16_json::Utf16JsonProjection>);
#[async_trait::async_trait]
impl lingxi_core::host::NewDiagnosticsSource for MockDiag {
    async fn take_new_diagnostics_block(
        &self,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        self.0.clone()
    }
}

fn orch_with_diag(
    source: Option<Arc<dyn lingxi_core::host::NewDiagnosticsSource>>,
) -> ConversationOrchestrator {
    let o = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        std::path::PathBuf::from("/work"),
    );
    match source {
        Some(s) => o.with_new_diagnostics_source(s),
        None => o,
    }
}

#[tokio::test]
async fn injects_block_when_source_has_new_diagnostics() {
    let block = "<new-diagnostics>The following new diagnostic issues were detected:\n\nx.rs:\n  \u{2718} [Line 1:1] boom</new-diagnostics>";
    let orch = orch_with_diag(Some(Arc::new(MockDiag(Some(block.into())))));
    let msg = orch
        .new_diagnostics_reminder_message()
        .await
        .expect("a block is injected");
    // The diagnostics attachment renders through the SAME batch wrapper as
    // every other reminder: oracle 2.1.238 @296693609 is
    //   case"diagnostics": … return Zy([kn({content: formatDiagnosticsBlock(n), isMeta:!0})])
    // and `Zy` maps `NT` = `<system-reminder>\n${e}\n</system-reminder>`.
    // `formatDiagnosticsBlock` itself returns the BARE block (@289741641),
    // which is why this test previously asserted the bare form — it was
    // reading the formatter and not the renderer that consumes it.
    assert_eq!(
        msg.text_content(),
        format!("<system-reminder>\n{block}\n</system-reminder>")
    );
}

#[tokio::test]
async fn no_reminder_without_source_or_when_empty() {
    // No source wired (the common no-LSP case).
    assert!(orch_with_diag(None)
        .new_diagnostics_reminder_message()
        .await
        .is_none());
    // Source wired but nothing new.
    assert!(orch_with_diag(Some(Arc::new(MockDiag(None))))
        .new_diagnostics_reminder_message()
        .await
        .is_none());
}

struct ConsumingDiagnostics {
    rows: std::sync::Mutex<
        std::collections::VecDeque<lingxi_core::types::utf16_json::Utf16JsonProjection>,
    >,
    calls: std::sync::atomic::AtomicUsize,
    cancel_after_drain: std::sync::atomic::AtomicBool,
    cancel: tokio_util::sync::CancellationToken,
}

#[async_trait::async_trait]
impl lingxi_core::host::NewDiagnosticsSource for ConsumingDiagnostics {
    async fn take_new_diagnostics_block(
        &self,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        use std::sync::atomic::Ordering;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let row = self.rows.lock().unwrap().pop_front();
        if row.is_some() && self.cancel_after_drain.swap(false, Ordering::SeqCst) {
            self.cancel.cancel();
        }
        row
    }
}

async fn check_consumed_diagnostics_recovery(streaming: bool, cancel_after_drain: bool, hot: bool) {
    use crate::test_support::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, mock_message_response, text_delta, MockStreamingApiClient,
    };
    use lingxi_core::host::FileSystem;
    use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let cwd = root.join("workspace");
    let home = root.join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(root));
    let session_id = lingxi_core::types::SessionId::new();
    let transcript = session::jsonl::path::session_path(
        &home,
        &cwd.to_string_lossy(),
        &session_id.as_uuid().to_string(),
    );
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let first =
        Source::parse(r#""<new-diagnostics>FIRST_DIAGNOSTIC\ud800</new-diagnostics>""#).unwrap();
    let second = Source::from("<new-diagnostics>SECOND_DIAGNOSTIC</new-diagnostics>");
    let cancel = tokio_util::sync::CancellationToken::new();
    let source = Arc::new(ConsumingDiagnostics {
        rows: std::sync::Mutex::new([first].into()),
        calls: AtomicUsize::new(0),
        cancel_after_drain: AtomicBool::new(cancel_after_drain),
        cancel: cancel.clone(),
    });
    let responses = (0..3)
        .map(|_| {
            let mut reply = mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            );
            reply.model = "claude-opus-4-7".into();
            reply
        })
        .collect();
    let api = Arc::new(MockApiClient::new(responses));
    let stream = Arc::new(MockStreamingApiClient::with_turns(
        (0..3)
            .map(|index| {
                vec![
                    message_start(&format!("diagnostics-{index}"), "claude-opus-4-7"),
                    content_block_start_text(0),
                    text_delta(0, "done"),
                    content_block_stop(0),
                    message_delta_stop("end_turn"),
                    message_stop(),
                ]
            })
            .collect(),
    ));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "claude-opus-4-7".into(),
                system_prompt_override: Some("diagnostics lifecycle".into()),
                ..Default::default()
            },
            api.clone(),
            stream.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_new_diagnostics_source(source.clone())
        .with_session_id(session_id)
        .with_config_home(home.clone())
        .with_jsonl_writer(Arc::new(session::jsonl::JsonlWriter::new(
            transcript.clone(),
            fs.clone(),
        ))),
    );
    orch.initialize_conversation_mode().await.unwrap();
    let outcome = if streaming {
        orch.run_turn_streaming_with_cancel("inspect diagnostics", cancel)
            .await
            .unwrap()
    } else {
        orch.run_turn_with_cancel("inspect diagnostics", cancel)
            .await
            .unwrap()
    };
    let calls_before = if streaming {
        stream.captured_calls().await.len()
    } else {
        api.captured_msgs().await.len()
    };
    let pending = orch.pending_diagnostics_reminder();
    if cancel_after_drain {
        assert!(matches!(outcome, crate::TurnOutcome::Cancelled));
        assert_eq!(calls_before, 0);
        let id = pending
            .as_ref()
            .expect("consumed diagnostic remains pending")
            .id();
        orch.acknowledge_step_reminders(&[]).await;
        orch.discard_pending_surfacing();
        assert_eq!(orch.pending_diagnostics_reminder().unwrap().id(), id);
        let already_cancelled = tokio_util::sync::CancellationToken::new();
        already_cancelled.cancel();
        if streaming {
            orch.run_turn_streaming_with_cancel("cancel again", already_cancelled)
                .await
                .unwrap();
        } else {
            orch.run_turn_with_cancel("cancel again", already_cancelled)
                .await
                .unwrap();
        }
        assert_eq!(orch.pending_diagnostics_reminder().unwrap().id(), id);
    } else {
        assert_eq!(calls_before, 1);
        assert!(
            pending.is_none(),
            "successful request ACKs the row (streaming={streaming}, hot={hot})"
        );
    }
    source.rows.lock().unwrap().push_back(second);
    if hot {
        let replayed = crate::resume::replay_session_state(
            &home,
            &cwd.to_string_lossy(),
            session_id.as_uuid(),
            fs.clone(),
        )
        .await
        .unwrap();
        let runtime = replayed.handle_runtime_snapshot();
        lingxi_core::host::OrchestratorHandle::resume_session(
            &*orch,
            session_id,
            replayed.state.history,
            replayed.last_message_uuid.map(|id| id.to_string()),
            None,
            runtime,
        )
        .await
        .unwrap();
        assert_eq!(
            orch.pending_diagnostics_reminder().unwrap().id(),
            pending.as_ref().unwrap().id(),
            "same-session hot remount retains the consumed row despite prompt generation reset"
        );
    }
    if cancel_after_drain {
        if streaming {
            orch.run_turn_streaming("continue after cancellation")
                .await
                .unwrap();
        } else {
            orch.run_turn("continue after cancellation").await.unwrap();
        }
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            1,
            "pending recovery does not drain newer diagnostics"
        );
        assert!(orch.pending_diagnostics_reminder().is_none());
    }
    for prompt in [
        "observe the next diagnostic",
        "unchanged diagnostics stay quiet",
    ] {
        if streaming {
            orch.run_turn_streaming(prompt).await.unwrap();
        } else {
            orch.run_turn(prompt).await.unwrap();
        }
    }
    let calls = if streaming {
        stream
            .captured_calls()
            .await
            .into_iter()
            .map(|call| call.messages)
            .collect::<Vec<_>>()
    } else {
        api.captured_msgs().await
    };
    assert_eq!(calls.len(), 3);
    for (index, marker) in [(0, "FIRST_DIAGNOSTIC"), (1, "SECOND_DIAGNOSTIC")] {
        let matching = calls[index]
            .iter()
            .filter(|row| row.text_content().contains(marker))
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        if index == 0 {
            if let Some(pending) = &pending {
                assert_eq!(
                    serde_json::to_vec(matching[0]).unwrap(),
                    serde_json::to_vec(pending).unwrap(),
                    "cancelled source retains exact UUID and UTF-16"
                );
            }
        }
        assert_eq!(
            calls
                .iter()
                .flatten()
                .filter(|row| row.text_content().contains(marker))
                .count(),
            1
        );
    }
    assert_eq!(source.calls.load(Ordering::SeqCst), 3);
    assert!(orch.pending_diagnostics_reminder().is_none());
    assert!(
        !std::fs::read_to_string(&transcript)
            .unwrap()
            .contains("_DIAGNOSTIC"),
        "transient diagnostics are not persisted into JSONL"
    );
    let replayed = crate::resume::replay_session_state(
        &home,
        &cwd.to_string_lossy(),
        session_id.as_uuid(),
        fs,
    )
    .await
    .unwrap();
    assert!(
        !replayed
            .state
            .history
            .iter()
            .any(|row| row.text_content().contains("_DIAGNOSTIC")),
        "cold history does not resurrect ACKed transient diagnostics"
    );
}

#[test]
fn diagnostics_keep_stable_pending_rows_through_cancel_and_ack_once_both_drivers() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    for streaming in [false, true] {
                        for (cancelled, hot) in [(false, false), (true, false), (true, true)] {
                            check_consumed_diagnostics_recovery(streaming, cancelled, hot).await;
                        }
                    }
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn a_different_session_does_not_inherit_consumed_pending_diagnostics_both_drivers() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    use crate::test_support::{
                        code_resume_runtime_snapshot, content_block_start_text, content_block_stop,
                        message_delta_stop, message_start, message_stop, mock_message_response,
                        text_delta, MockStreamingApiClient,
                    };
                    use std::sync::atomic::{AtomicBool, AtomicUsize};
                    for streaming in [false, true] {
                        let temp = tempfile::tempdir().unwrap();
                        let cancel = tokio_util::sync::CancellationToken::new();
                        let source = Arc::new(ConsumingDiagnostics {
                            rows: std::sync::Mutex::new(
                                ["<new-diagnostics>OLD_SESSION_DIAGNOSTIC</new-diagnostics>"
                                    .into()]
                                .into(),
                            ),
                            calls: AtomicUsize::new(0),
                            cancel_after_drain: AtomicBool::new(true),
                            cancel: cancel.clone(),
                        });
                        let api = Arc::new(MockApiClient::new(vec![mock_message_response(
                            vec![llm_runtime::ContentBlock::Text {
                                text: "done".into(),
                                cache_control: None,
                                citations: None,
                            }],
                            Some("end_turn"),
                        )]));
                        let stream = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
                            message_start("new-session", "claude-opus-4-7"),
                            content_block_start_text(0),
                            text_delta(0, "done"),
                            content_block_stop(0),
                            message_delta_stop("end_turn"),
                            message_stop(),
                        ]]));
                        let orch = ConversationOrchestrator::into_shared(
                            ConversationOrchestrator::new_with_streaming(
                                OrchestratorConfig {
                                    model: "claude-opus-4-7".into(),
                                    system_prompt_override: Some(
                                        "session diagnostic ownership".into(),
                                    ),
                                    ..Default::default()
                                },
                                api.clone(),
                                stream.clone(),
                                Arc::new(ToolRegistry::new()),
                                noop_hook_executor(),
                                Arc::new(NoOpPermissionGate),
                                Arc::new(MockOutputStream::new()),
                                Arc::new(StaticMemoryProvider::empty()),
                                temp.path().to_owned(),
                            )
                            .with_new_diagnostics_source(source),
                        );
                        if streaming {
                            orch.run_turn_streaming_with_cancel("old session", cancel)
                                .await
                                .unwrap();
                        } else {
                            orch.run_turn_with_cancel("old session", cancel)
                                .await
                                .unwrap();
                        }
                        assert!(orch.pending_diagnostics_reminder().is_some());
                        let other = lingxi_core::types::SessionId::new();
                        lingxi_core::host::OrchestratorHandle::resume_session(
                            &*orch,
                            other,
                            Vec::new(),
                            None,
                            None,
                            code_resume_runtime_snapshot(other),
                        )
                        .await
                        .unwrap();
                        assert!(orch.pending_diagnostics_reminder().is_none());
                        if streaming {
                            orch.run_turn_streaming("new session request")
                                .await
                                .unwrap();
                        } else {
                            orch.run_turn("new session request").await.unwrap();
                        }
                        let calls = if streaming {
                            stream
                                .captured_calls()
                                .await
                                .into_iter()
                                .map(|call| call.messages)
                                .collect::<Vec<_>>()
                        } else {
                            api.captured_msgs().await
                        };
                        assert_eq!(calls.len(), 1);
                        assert!(!calls[0]
                            .iter()
                            .any(|row| row.text_content().contains("OLD_SESSION_DIAGNOSTIC")));
                    }
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn synthetic_mod_response_does_not_ack_diagnostics_before_a_real_response_both_drivers() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            use crate::test_support::{mock_message_response, content_block_start_text,
                content_block_stop, message_delta_stop, message_start, message_stop, text_delta,
                MockStreamingApiClient};
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            for streaming in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let module = temp.path().join("synthetic-first.js");
                std::fs::write(&module, r#"export function register(on) {
                  let first = true;
                  on('turn.step', async function* ($, e, next) {
                    if (first) {
                      first = false;
                      yield {kind:'text', index:0, text:'synthetic answer'};
                      yield {kind:'stop', stopReason:'end_turn', usage:null};
                      return {turnId:e.turnId, index:e.index, answer:'synthetic answer', toolUses:[], stopReason:'end_turn', usage:null};
                    }
                    const below = next(e);
                    for await (const chunk of below) yield chunk;
                    return await below.result;
                  });
                }"#).unwrap();
                let host = hooks::mods::ModHost::start(None).await.unwrap();
                host.load("synthetic-first", temp.path(), &module,
                    tool_api::tool_trait::generated_builtin_result(&serde_json::json!({}))).await.unwrap();
                let mut registry = hooks::HookRegistry::new();
                registry.set_mod_host(host);
                let source = Arc::new(ConsumingDiagnostics {
                    rows: std::sync::Mutex::new(["<new-diagnostics>SYNTHETIC_PENDING_DIAGNOSTIC</new-diagnostics>".into()].into()),
                    calls: AtomicUsize::new(0), cancel_after_drain: AtomicBool::new(false),
                    cancel: tokio_util::sync::CancellationToken::new(),
                });
                let api = Arc::new(MockApiClient::new((0..2).map(|_| mock_message_response(
                    vec![llm_runtime::ContentBlock::Text {text:"real answer".into(), cache_control:None, citations:None}],
                    Some("end_turn"),
                )).collect()));
                let stream = Arc::new(MockStreamingApiClient::with_turns((0..2).map(|index| vec![
                    message_start(&format!("physical-after-synthetic-{index}"), "claude-opus-4-7"),
                    content_block_start_text(0), text_delta(0,"real answer"), content_block_stop(0),
                    message_delta_stop("end_turn"), message_stop(),
                ]).collect()));
                let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                    OrchestratorConfig {model:"claude-opus-4-7".into(), system_prompt_override:Some("synthetic diagnostic ACK".into()), ..Default::default()},
                    api.clone(), stream.clone(), Arc::new(ToolRegistry::new()), noop_hook_executor(),
                    Arc::new(NoOpPermissionGate), Arc::new(MockOutputStream::new()),
                    Arc::new(StaticMemoryProvider::empty()), temp.path().to_owned(),
                ).with_new_diagnostics_source(source.clone())
                    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))));
                if streaming { orch.run_turn_streaming("synthetic first").await.unwrap(); }
                else { orch.run_turn("synthetic first").await.unwrap(); }
                assert!(api.captured_msgs().await.is_empty());
                assert!(stream.captured_calls().await.is_empty());
                let pending = orch.pending_diagnostics_reminder().expect("synthetic output cannot ACK an unsent diagnostic");
                for prompt in ["real response next", "unchanged diagnostics stay quiet"] {
                    if streaming { orch.run_turn_streaming(prompt).await.unwrap(); }
                    else { orch.run_turn(prompt).await.unwrap(); }
                }
                let requests = if streaming {
                    stream.captured_calls().await.into_iter().map(|call|call.messages).collect::<Vec<_>>()
                } else {api.captured_msgs().await};
                assert_eq!(requests.len(), 2);
                let retained = requests[0].iter().filter(|row|row.id()==pending.id()).collect::<Vec<_>>();
                assert_eq!(retained.len(), 1);
                assert_eq!(serde_json::to_vec(retained[0]).unwrap(), serde_json::to_vec(&pending).unwrap());
                assert!(!requests[1].iter().any(|row|row.id()==pending.id()));
                assert!(orch.pending_diagnostics_reminder().is_none());
                assert_eq!(source.calls.load(Ordering::SeqCst), 2, "only a successful physical response releases the source cursor");
            }
        });
    }).unwrap().join().unwrap();
}

#[test]
fn diagnostic_ack_follows_completed_sources_when_mods_replace_or_catch_later_calls_both_drivers() {
    std::thread::Builder::new().stack_size(32 * 1024 * 1024).spawn(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            use crate::test_support::{mock_message_response, content_block_start_text,
                content_block_stop, message_delta_stop, message_start, message_stop, text_delta,
                MockStreamingApiClient};
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            for streaming in [false, true] {
                for mode in ["success_replace", "success_then_failure", "failure_replace"] {
                    let temp = tempfile::tempdir().unwrap();
                    let module = temp.path().join("completed-source.js");
                    let script = r#"export function register(on) {
                      const mode = MODE;
                      let first = true;
                      on('turn.step', async function* ($, e, next) {
                        if (!first) {
                          const pass = next(e);
                          for await (const chunk of pass) yield chunk;
                          return await pass.result;
                        }
                        first = false;
                        const below = next(e);
                        try { for await (const chunk of below) {} await below.result; }
                        catch (error) { if (mode !== 'failure_replace') throw error; }
                        if (mode === 'success_then_failure') {
                          const later = next(e);
                          try { for await (const chunk of later) {} await later.result; }
                          catch (error) {}
                        }
                        yield {kind:'text', index:0, text:'synthetic replacement'};
                        yield {kind:'stop', stopReason:'end_turn', usage:null};
                        return {turnId:e.turnId,index:e.index,answer:'synthetic replacement',toolUses:[],stopReason:'end_turn',usage:null};
                      });
                    }"#.replace("MODE", &serde_json::to_string(mode).unwrap());
                    std::fs::write(&module, script).unwrap();
                    let host = hooks::mods::ModHost::start(None).await.unwrap();
                    host.load("completed-source", temp.path(), &module,
                        tool_api::tool_trait::generated_builtin_result(&serde_json::json!({}))).await.unwrap();
                    let mut registry = hooks::HookRegistry::new();
                    registry.set_mod_host(host);
                    let source = Arc::new(ConsumingDiagnostics {
                        rows: std::sync::Mutex::new(["<new-diagnostics>COMPLETED_SOURCE_DIAGNOSTIC</new-diagnostics>".into()].into()),
                        calls: AtomicUsize::new(0), cancel_after_drain: AtomicBool::new(false),
                        cancel: tokio_util::sync::CancellationToken::new(),
                    });
                    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
                        vec![llm_runtime::ContentBlock::Text {text:"physical answer".into(),cache_control:None,citations:None}],
                        Some("end_turn"),
                    )]));
                    let failure = llm_runtime::LlmError::Transport {message:"fixture failed next".into()};
                    if mode == "failure_replace" { api.set_fail_with(Some(failure.clone())); }
                    let success = vec![message_start("completed-source", "claude-opus-4-7"),
                        content_block_start_text(0), text_delta(0,"physical answer"), content_block_stop(0),
                        message_delta_stop("end_turn"), message_stop()];
                    let scripts = if mode == "failure_replace" {
                        vec![vec![Err(failure.clone())], success.into_iter().map(Ok).collect()]
                    } else if mode == "success_then_failure" {
                        vec![success.into_iter().map(Ok).collect(), vec![Err(failure)]]
                    } else {vec![success.into_iter().map(Ok).collect()]};
                    let stream = Arc::new(MockStreamingApiClient::with_fallible_turns(scripts));
                    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                        OrchestratorConfig {model:"claude-opus-4-7".into(), system_prompt_override:Some("completed diagnostics".into()), ..Default::default()},
                        api.clone(), stream.clone(), Arc::new(ToolRegistry::new()), noop_hook_executor(),
                        Arc::new(NoOpPermissionGate), Arc::new(MockOutputStream::new()),
                        Arc::new(StaticMemoryProvider::empty()), temp.path().to_owned(),
                    ).with_new_diagnostics_source(source.clone())
                        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))));
                    if streaming { orch.run_turn_streaming("replace visible result").await.unwrap(); }
                    else { orch.run_turn("replace visible result").await.unwrap(); }
                    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
                    if mode == "failure_replace" {
                        let pending = orch.pending_diagnostics_reminder().expect("failed next plus synthetic replacement cannot ACK");
                        api.set_fail_with(None);
                        if streaming { orch.run_turn_streaming("real success after failed next").await.unwrap(); }
                        else { orch.run_turn("real success after failed next").await.unwrap(); }
                        let requests = if streaming {
                            stream.captured_calls().await.into_iter().map(|call|call.messages).collect::<Vec<_>>()
                        } else {api.captured_msgs().await};
                        assert!(requests.last().unwrap().iter().any(|row| row.id() == pending.id()));
                        assert_eq!(source.calls.load(Ordering::SeqCst), 1, "pending failed source is replayed without reconsuming");
                    }
                    assert!(orch.pending_diagnostics_reminder().is_none(), "completed source must ACK even when its visible result was replaced; {mode}, streaming={streaming}");
                }
            }
        });
    }).unwrap().join().unwrap();
}
