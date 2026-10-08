use super::*;
use crate::prompt::MemoryFile;
use crate::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
    mock_message_response, noop_hook_executor,
};
use crate::OrchestratorConfig;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ContentBlock;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn orch_with(memory: Arc<StaticMemoryProvider>, email: Option<&str>) -> ConversationOrchestrator {
    orch_with_rendering(
        memory,
        email,
        lingxi_core::host::instructions::InstructionRendering::Inline,
    )
}

fn orch_with_rendering(
    memory: Arc<StaticMemoryProvider>,
    email: Option<&str>,
    context_rendering: lingxi_core::host::instructions::InstructionRendering,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig {
            user_email: email.map(str::to_string),
            context_rendering,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory,
        std::env::temp_dir(),
    )
}

fn text(msg: &ConversationMessage) -> String {
    match msg {
        ConversationMessage::User { content, .. } => content
            .iter()
            .filter_map(ContentBlock::visible_text)
            .collect(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn mod_ui_control_without_mods_preserves_the_canonical_empty_render() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let props = serde_json::json!({"label":"unchanged", "nested":{"count":3}});
    let outcome = lingxi_core::host::OrchestratorHandle::mod_ui_control(
        &orch,
        Utf16JsonProjection::plain(serde_json::json!({
            "subtype":"ui_render", "surface":"desktop", "component":"AbovePrompt",
            "instance_id":"above-main", "props":props,
            "viewport":{"columns":80,"rows":24}, "unknown_protocol_field":"discarded"
        })),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.response,
        Utf16JsonProjection::plain(
            serde_json::json!({"tree":null,"props":props,"rewritten":false,"hooked":false})
        )
    );
    assert_eq!(outcome.render_revision, None);
    assert!(outcome.client_runtime_epochs.is_empty());
    assert!(outcome.client_state_token.is_none());
    assert!(orch.session.lock().await.history.is_empty());
}

#[tokio::test]
async fn mod_ui_control_validates_requests_even_without_a_mod_worker() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    for request in [
        Utf16JsonProjection::plain(serde_json::json!({
            "subtype":"ui_render", "surface":"desktop", "component":"AbovePrompt",
            "instance_id":"above-main", "props":[]
        })),
        Utf16JsonProjection::plain(serde_json::json!({
            "subtype":"ui_client_press", "plugin":"caller", "component":"AbovePrompt",
            "instance_id":"above-main", "client":"counter", "module":"counter.tsx",
            "element":"increment", "event":{"type":"input","kind":"other","value":"x"}
        })),
    ] {
        assert!(
            lingxi_core::host::OrchestratorHandle::mod_ui_control(&orch, request)
                .await
                .is_err()
        );
    }
    assert!(orch.session.lock().await.history.is_empty());
}

#[tokio::test]
async fn plugin_prompt_context_attachment_is_screened_with_plugin_origin() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("plugin-context-attachment.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('prompt.attachment', { type: 'hook_additional_context' }, ($, e) => {
            if (e.origin.kind !== 'plugin' || e.origin.event !== 'prompt.submit') {
              throw new Error('wrong plugin origin');
            }
            return { text: 'PLUGIN REWRITTEN' };
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("plugin-context", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    orch.append_mod_prompt_context(&["ORIGINAL".into()]).await;
    let original = orch.session.lock().await.history.last().cloned().unwrap();
    assert!(text(&original).contains("prompt.submit hook additional context: ORIGINAL"));
    let prepared = orch
        .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
        .await
        .unwrap();
    assert!(
        prepared
            .snapshot
            .iter()
            .any(|message| text(message)
                == "<system-reminder>\nPLUGIN REWRITTEN\n</system-reminder>")
    );
    assert!(
        orch.session
            .lock()
            .await
            .history
            .iter()
            .any(|message| message == &original)
    );
}

#[tokio::test]
async fn mod_turn_start_fires_once_after_prompt_screening_in_all_main_drivers() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("turn-start.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('prompt.submit', ($, e, next) => e.text === 'drop'
            ? { drop: 'screened out' } : next({ ...e, text: 'rewritten' }));
          on('turn.start', ($, e, next) => {
            $.ui.log(`${e.text}:${e.turnId}`, { to: 'transcript' });
            return { turnId: 'ignored-hook-result' };
          });
        }"#,
    )
    .unwrap();
    for driver in ["batched", "cancelable", "streaming"] {
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("turn-start", dir.path(), &module, serde_json::json!({}))
            .await
            .unwrap();
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        let output = Arc::new(MockOutputStream::new());
        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
            message_start("msg_turn_start", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "ok"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![mock_message_response(
                    vec![llm_runtime::ContentBlock::Text {
                        text: "ok".into(),
                        cache_control: None,
                        citations: None,
                    }],
                    Some("end_turn"),
                )])),
                streaming,
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                dir.path().to_path_buf(),
            )
            .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
        );
        match driver {
            "batched" => {
                orch.run_turn("original").await.unwrap();
                orch.run_turn("drop").await.unwrap();
            }
            "cancelable" => {
                orch.run_turn_with_cancel("original", tokio_util::sync::CancellationToken::new())
                    .await
                    .unwrap();
                orch.run_turn_with_cancel("drop", tokio_util::sync::CancellationToken::new())
                    .await
                    .unwrap();
            }
            "streaming" => {
                orch.run_turn_streaming("original").await.unwrap();
                orch.run_turn_streaming("drop").await.unwrap();
            }
            _ => unreachable!(),
        }
        let logs: Vec<_> = output
            .snapshot()
            .await
            .into_iter()
            .filter_map(|event| match event {
                lingxi_core::host::OutputEvent::ModLog { plugin, text }
                    if plugin == "turn-start" =>
                {
                    Some(text)
                }
                _ => None,
            })
            .collect();
        assert_eq!(logs.len(), 1, "{driver}: {logs:?}");
        let turn_id = logs[0].strip_prefix("rewritten:").unwrap();
        uuid::Uuid::parse_str(turn_id).unwrap();
    }
}

#[tokio::test]
async fn mod_turn_complete_observes_raw_answer_and_shows_separate_notice() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("turn-complete.js");
    std::fs::write(
        &module,
        r#"let started;
        export function register(on) {
          on('turn.start', ($, e, next) => { started = e.turnId; return next(e); });
          on('turn.complete', ($, e, next) => {
            if (e.turnId !== started || e.answer !== 'original reply'
                || e.reason !== 'answer' || e.isAborted !== false
                || e.usage?.model !== 'claude-opus-4-7') {
              throw new Error('turn.complete saw the wrong turn');
            }
            return { text: 'extra context' };
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("complete-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "original reply".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    orch.run_turn("question").await.unwrap();
    let notices: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::SystemNotice { body, .. } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(notices, vec!["complete-mod: extra context".to_string()]);
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(
        message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "original reply"))
    )));
    assert!(!format!("{history:?}").contains("extra context"));
}

#[tokio::test]
async fn mod_turn_complete_reports_api_error_without_usage() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("turn-error.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.complete', ($, e, next) => {
            $.ui.log(`${e.reason}:${e.answer}:${e.usage === undefined}`, { to: 'transcript' });
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("error-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_fail_with(Some(llm_runtime::LlmError::Authentication {
        message: "bad credential".into(),
    }));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let _ = orch.run_turn("question").await;
    let logs: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { plugin, text } if plugin == "error-mod" => {
                Some(text)
            }
            _ => None,
        })
        .collect();
    assert_eq!(logs, vec!["error::true".to_string()]);
}

#[tokio::test]
async fn mod_turn_complete_runs_after_streamed_answer() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("stream-complete.js");
    std::fs::write(
        &module,
        r#"let started;
        export function register(on) {
          on('turn.start', ($, e, next) => { started = e.turnId; return next(e); });
          on('turn.complete', ($, e, next) => {
            $.ui.log(`${e.turnId === started}:${e.reason}:${e.answer}`, { to: 'transcript' });
            return { text: 'stream summary' };
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("stream-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let output = Arc::new(MockOutputStream::new());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
        message_start("msg_complete", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "streamed reply"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    orch.run_turn_streaming("question").await.unwrap();
    let events = output.snapshot().await;
    assert!(events.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { plugin, text }
            if plugin == "stream-mod" && text == "true:answer:streamed reply"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::SystemNotice { body, .. }
            if body == "stream-mod: stream summary"
    )));
}

#[tokio::test]
async fn mod_turn_step_rewrites_live_model_stream_and_persisted_text() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("turn-step.js");
    std::fs::write(
        &module,
        r#"let seen;
        export function register(on) {
          on('prompt.submit', ($, e, next) => e.text === 'probe'
            ? next({ ...e, text: JSON.stringify(seen ?? null) }) : next(e));
          on('turn.step', async function* ($, e, next) {
            if (e.index !== 0 || e.messageCount < 1) throw new Error('wrong step envelope');
            if (e.effort !== 'high') throw new Error('wrong effort');
            const below = next({ ...e, model: 'hook-model', effort: 'low' });
            for await (const chunk of below) {
              yield chunk.kind === 'text' ? { ...chunk, text: chunk.text.toUpperCase() } : chunk;
            }
            const result = await below.result;
            if (result.answer !== 'hooked' || result.stopReason !== 'end_turn') {
              throw new Error(`wrong streamed result: ${JSON.stringify(result)}`);
            }
            seen = result;
            return { ...result, answer: 'HOOKED' };
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("step-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
        message_start("msg_step", "hook-model"),
        content_block_start_text(0),
        text_delta(0, "hooked"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    orch.set_effort(Some("high".into()));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("turn.step stream must make progress")
    .unwrap();
    assert_eq!(streaming.captured_calls().await[0].model, "hook-model");
    assert_eq!(
        streaming.captured_calls().await[0]
            .effort_override
            .as_deref(),
        Some("low")
    );
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(
        message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "HOOKED"))
    )));
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let result = host
                .dispatch(
                    "prompt.submit",
                    serde_json::json!({"text":"probe"}),
                    |event| async move { Ok(event) },
                )
                .await
                .unwrap();
            let result: serde_json::Value =
                serde_json::from_str(result["text"].as_str().unwrap()).unwrap();
            if !result.is_null() {
                break result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("main streamed next.result settles");
    assert_eq!(result["answer"], "hooked");
    assert_eq!(result["stopReason"], "end_turn");
}

#[tokio::test]
async fn mod_turn_step_stream_keeps_synthetic_and_physical_assistant_records() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("stream-two-records.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* ($, e, next) {
            yield { kind: 'text', index: 0, text: 'synthetic first' };
            const below = next({ ...e, model: 'hook-model' });
            for await (const chunk of below) yield chunk;
            return await below.result;
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "stream-two-records",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
        message_start("msg_physical", "hook-model"),
        content_block_start_text(0),
        text_delta(0, "physical second"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("two streamed assistant records complete")
    .unwrap();
    assert_eq!(streaming.captured_calls().await.len(), 1);
    assert_eq!(streaming.captured_calls().await[0].model, "hook-model");
    let texts: Vec<String> = orch
        .session
        .lock()
        .await
        .history
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["synthetic first", "physical second"]);
}

#[tokio::test]
async fn mod_turn_step_stream_handles_two_physical_next_calls_in_one_step() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("stream-two-next.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* ($, e, next) {
            const first = next(e);
            for await (const chunk of first) yield chunk;
            await first.result;
            const second = next(e);
            for await (const chunk of second) yield chunk;
            return await second.result;
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "stream-two-next",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let response = |id, text| {
        crate::scripted![
            message_start(id, "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, text),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]
    };
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
        response("msg_first", "first"),
        response("msg_second", "second"),
    ]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("two physical next calls complete")
    .unwrap();
    assert_eq!(streaming.captured_calls().await.len(), 2);
    let texts: Vec<String> = orch
        .session
        .lock()
        .await
        .history
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["first", "second"]);
}

#[tokio::test]
async fn mod_turn_step_stream_finishes_earlier_tool_before_later_answer() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("stream-tool-then-answer.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* ($, e, next) {
            yield { kind: 'tool', index: 0, id: 'toolu_mod_first', name: 'UnknownModTool' };
            yield { kind: 'input', index: 0, json: '{}' };
            yield { kind: 'stop', stopReason: 'tool_use', usage: null };
            const below = next(e);
            for await (const chunk of below) yield chunk;
            return await below.result;
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "stream-tool-then-answer",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
        message_start("msg_after_tool", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "after tool"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("tool result and later streamed answer complete")
    .unwrap();
    assert_eq!(streaming.captured_calls().await.len(), 1);
    let history = &orch.session.lock().await.history;
    let tool = history
        .iter()
        .position(|message| {
            matches!(message,
                ConversationMessage::Assistant { content, .. }
                    if content.iter().any(|block| matches!(block,
                        ContentBlock::ToolUse { name, .. } if name == "UnknownModTool"))
            )
        })
        .expect("synthetic tool response");
    let result = history
        .iter()
        .position(|message| {
            matches!(message,
                ConversationMessage::User { content, .. }
                    if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. }))
            )
        })
        .expect("tool result");
    let answer = history
        .iter()
        .position(|message| {
            matches!(message,
                ConversationMessage::Assistant { content, .. }
                    if content.iter().any(|block| matches!(block,
                        ContentBlock::Text { text, .. } if text == "after tool"))
            )
        })
        .expect("later answer");
    assert!(tool < result && result < answer);
}

#[tokio::test]
async fn mod_turn_step_stream_hook_throw_before_next_uses_core_request() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("step-before-next-error.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* () {
            throw new Error('failure before next');
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("step-error", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let config = OrchestratorConfig::default();
    let original_model = config.model.clone();
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
        message_start("msg_original", &original_model),
        content_block_start_text(0),
        text_delta(0, "original answer"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            config,
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("pre-next failure falls back to provider")
    .unwrap();
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, original_model);
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(
        message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "original answer"))
    )));
}

#[tokio::test]
async fn mod_turn_step_synthetic_response_skips_provider_request() {
    use crate::test_support_stream::MockStreamingApiClient;

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("synthetic-step.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* ($, e) {
            yield { kind: 'text', index: 0, text: 'synthetic answer' };
            yield { kind: 'stop', stopReason: 'end_turn', usage: null };
            return { turnId: e.turnId, index: e.index, answer: 'synthetic answer',
              toolUses: [], stopReason: 'end_turn', usage: null };
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("synthetic-step", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let streaming = Arc::new(MockStreamingApiClient::empty());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("synthetic turn.step stream must make progress")
    .unwrap();
    assert!(streaming.captured_calls().await.is_empty());
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(
        message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "synthetic answer"))
    )));
}

#[tokio::test]
async fn mod_turn_step_thinking_rewrite_changes_display_but_keeps_signed_history() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_start_thinking,
        content_block_stop, message_delta_stop, message_start, message_stop, text_delta,
        thinking_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("thinking-step.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* ($, e, next) {
            const below = next(e);
            for await (const chunk of below) {
              yield chunk.kind === 'thinking' ? { ...chunk, text: 'shown thought' } : chunk;
            }
            return await below.result;
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("thinking-step", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let output = Arc::new(MockOutputStream::new());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("msg_thinking", "claude-opus-4-7"),
        content_block_start_thinking(0),
        thinking_delta(0, "signed thought"),
        llm_runtime::HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: llm_runtime::HistoryContentDelta::SignatureDelta {
                signature: "sig_1".into(),
            },
        },
        content_block_stop(0),
        content_block_start_text(1),
        text_delta(1, "answer"),
        content_block_stop(1),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("signed-thinking turn.step stream must make progress")
    .unwrap();
    let shown: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::Thinking { thinking, .. } => Some(thinking),
            _ => None,
        })
        .collect();
    assert_eq!(shown, vec!["shown thought"]);
    let history = &orch.session.lock().await.history;
    assert!(
        history.iter().any(|message| matches!(message,
            ConversationMessage::Assistant { content, .. }
                if content.iter().any(|block| matches!(block,
                    ContentBlock::Thinking { thinking, signature }
                        if thinking == "signed thought" && signature.as_deref() == Some("sig_1")))
        )),
        "history: {history:?}"
    );
}

#[tokio::test]
async fn mod_turn_step_dropped_tool_chunk_never_dispatches_tool() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_tool_use, content_block_stop, input_json_delta,
        message_delta_stop, message_start, message_stop,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("drop-tool-step.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.step', async function* ($, e, next) {
            const below = next(e);
            for await (const chunk of below) {
              if (chunk.kind === 'tool' || chunk.kind === 'input') continue;
              if (chunk.kind === 'stop') {
                yield { kind: 'text', index: 1, text: 'tool omitted' };
                yield { ...chunk, stopReason: 'end_turn' };
              } else yield chunk;
            }
            return await below.result;
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("drop-tool-step", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("msg_drop_tool", "claude-opus-4-7"),
        content_block_start_tool_use(0, lingxi_core::types::ToolUseId::new(), "Bash"),
        input_json_delta(0, "{\"command\":\"echo wrong\"}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn_streaming("question"),
    )
    .await
    .expect("dropped-tool turn.step stream must make progress")
    .unwrap();
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "tool omitted"))
    )));
    assert!(!history.iter().any(|message| matches!(message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    )));
}

#[tokio::test]
async fn mod_turn_step_batched_keeps_synthetic_and_physical_assistant_records() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-two-records.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        yield { kind: 'text', index: 0, text: 'synthetic first' };
        const below = next(e);
        for await (const chunk of below) yield chunk;
        return await below.result;
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "batch-two-records",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "physical second".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn("question"),
    )
    .await
    .expect("two assistant records complete")
    .unwrap();
    assert_eq!(api.captured_msgs().await.len(), 1);
    let history = &orch.session.lock().await.history;
    let texts: Vec<_> = history
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["synthetic first", "physical second"]);
}

#[tokio::test]
async fn mod_turn_step_batched_dispatches_an_earlier_tool_before_the_final_response() {
    use tool_api::context::ToolUseContext;
    use tool_api::progress::ToolProgressSender;
    use tool_api::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
        ValidationError,
    };

    struct Probe(std::sync::Mutex<Vec<serde_json::Value>>);
    #[async_trait::async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            "Probe"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| serde_json::json!({"type":"object"}))
        }
        fn is_enabled(&self, _: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _: &serde_json::Value) -> bool {
            true
        }
        async fn validate_input(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> Result<(), ValidationError> {
            Ok(())
        }
        async fn check_permissions(
            &self,
            _: &serde_json::Value,
            _: &ToolUseContext,
        ) -> permission::PermissionResult {
            permission::PermissionResult::Allow {
                reason: permission::PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: permission::result::PermissionMetadata::default(),
            }
        }
        async fn description(&self, _: &serde_json::Value, _: &DescriptionOptions) -> String {
            "probe".into()
        }
        async fn prompt(&self, _: &PromptOptions) -> String {
            "probe".into()
        }
        async fn call(
            &self,
            input: serde_json::Value,
            _: ToolUseContext,
            _: ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            self.0.lock().unwrap().push(input);
            Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
                data: serde_json::json!({"content":"probe done"}),
                model_content: None,
                new_messages: Vec::new(),
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-tool-then-answer.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        yield { kind: 'tool', index: 0, id: 'toolu_mod_first', name: 'Probe' };
        yield { kind: 'input', index: 0, json: '{"path":"first"}' };
        yield { kind: 'stop', stopReason: 'tool_use', usage: null };
        const below = next(e);
        for await (const chunk of below) yield chunk;
        return await below.result;
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "batch-tool-then-answer",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut hooks = hooks::HookRegistry::new();
    hooks.set_mod_host(host);
    let probe = Arc::new(Probe(std::sync::Mutex::new(Vec::new())));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(probe.clone());
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "after tool".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hooks)));
    let orch = ConversationOrchestrator::into_shared(orch);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn("question"),
    )
    .await
    .expect("tool and final response complete")
    .unwrap();
    assert_eq!(api.captured_msgs().await.len(), 1);
    assert_eq!(
        *probe.0.lock().unwrap(),
        vec![serde_json::json!({"path":"first"})]
    );
    let history = &orch.session.lock().await.history;
    let tool_position = history.iter().position(|message| matches!(message,
        ConversationMessage::Assistant { content, .. } if content.iter().any(|block| matches!(block,
            ContentBlock::ToolUse { name, .. } if name == "Probe"))
    )).expect("first response has the tool call");
    let result_position = history.iter().position(|message| matches!(message,
        ConversationMessage::User { content, .. } if content.iter().any(|block| matches!(block,
            ContentBlock::ToolResult { .. }))
    )).expect("tool result follows");
    let answer_position = history.iter().position(|message| matches!(message,
        ConversationMessage::Assistant { content, .. } if content.iter().any(|block| matches!(block,
            ContentBlock::Text { text, .. } if text == "after tool"))
    )).expect("final response follows");
    assert!(tool_position < result_position && result_position < answer_position);
}

#[tokio::test]
async fn mod_turn_step_batched_passes_opaque_response_and_rewrites_model() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-step.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        const below = next({ ...e, model: 'batch-hook-model' });
        for await (const chunk of below) {
          if (chunk.kind !== 'engine') throw new Error('non-stream response is opaque');
          yield chunk;
        }
        return await below.result;
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("batch-step", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "raw batch".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    orch.run_turn("question").await.unwrap();
    assert_eq!(api.captured_models().await, vec!["batch-hook-model"]);
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "raw batch"))
    )));
}

#[tokio::test]
async fn mod_turn_step_batched_synthetic_answer_skips_provider() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-synthetic.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e) {
        yield { kind: 'text', index: 0, text: 'batch synthetic' };
        yield { kind: 'stop', stopReason: 'end_turn', usage: null };
        return { turnId: e.turnId, index: e.index, answer: 'batch synthetic',
          toolUses: [], stopReason: 'end_turn', usage: null };
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "batch-synthetic",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let api = Arc::new(MockApiClient::new(vec![]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    orch.run_turn("question").await.unwrap();
    assert!(api.captured_msgs().await.is_empty());
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "batch synthetic"))
    )));
}

#[tokio::test]
async fn mod_turn_step_batched_replacement_keeps_physical_usage() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-replacement.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        const below = next(e);
        for await (const _chunk of below) {}
        yield { kind: 'text', index: 0, text: 'replacement' };
        yield { kind: 'stop', stopReason: 'end_turn', usage: null };
        return { ...await below.result, answer: 'replacement' };
      });
      on('turn.complete', ($, e, next) => {
        $.ui.log(`usage:${e.usage?.input_tokens}:${e.usage?.output_tokens}`);
        return next(e);
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "batch-replacement",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let mut physical = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "physical".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    physical.usage.counts_mut().input_tokens = 11;
    physical.usage.counts_mut().output_tokens = 7;
    let api = Arc::new(MockApiClient::new(vec![physical]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    orch.run_turn("question").await.unwrap();
    let history = &orch.session.lock().await.history;
    assert!(history.iter().any(|message| matches!(message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block,
                ContentBlock::Text { text, .. } if text == "replacement"))
    )));
    assert!(!format!("{history:?}").contains("physical"));
    let logs: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        logs.iter().any(|text| text == "usage:11:7"),
        "logs: {logs:?}"
    );
}

#[tokio::test]
async fn mod_turn_step_batched_multiple_next_calls_count_every_provider_response() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-multiple-next.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        const first = next({ ...e, model: 'batch-first' });
        for await (const _chunk of first) {}
        const second = next({ ...e, model: 'batch-second' });
        for await (const chunk of second) yield chunk;
        return await second.result;
      });
      on('turn.complete', ($, e, next) => {
        $.ui.log(`usage:${e.usage?.input_tokens}:${e.usage?.output_tokens}:${e.usage?.model}`);
        return next(e);
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "batch-multiple-next",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let mut first = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "first response is hidden".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    first.usage.counts_mut().input_tokens = 11;
    first.usage.counts_mut().output_tokens = 7;
    let mut second = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "second response is shown".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    second.usage.counts_mut().input_tokens = 13;
    second.usage.counts_mut().output_tokens = 5;
    let api = Arc::new(MockApiClient::new(vec![first, second]));
    let output = Arc::new(MockOutputStream::new());
    let base = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let session_id = base.session.lock().await.session_id;
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::channel(8);
    let tracker = Arc::new(cost::CostTracker::new(
        session_id,
        Arc::new(cost::PricingCatalog::builtin_reference()),
        persist_tx,
    ));
    let orch = base.with_cost_tracker(tracker.clone());
    orch.run_turn("question").await.unwrap();
    assert_eq!(
        api.captured_models().await,
        vec!["batch-first", "batch-second"]
    );
    let history = &orch.session.lock().await.history;
    assert!(format!("{history:?}").contains("second response is shown"));
    assert!(!format!("{history:?}").contains("first response is hidden"));
    let logs: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        logs.iter()
            .any(|text| text == "usage:24:12:claude-opus-4-7"),
        "logs: {logs:?}"
    );
    assert_eq!(tracker.snapshot().await.cost_revision, 2);
    assert_eq!(
        orch.model_runtime
            .api_calls_recorded
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[tokio::test]
async fn mod_turn_step_concurrent_next_results_keep_their_own_response() {
    struct ByModel;
    #[async_trait::async_trait]
    impl OrchestratorApiClient for ByModel {
        async fn messages_create(
            &self,
            request: crate::OrchestratorApiRequest,
        ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
            let model = match request {
                crate::OrchestratorApiRequest::Main(request) => request.model,
                crate::OrchestratorApiRequest::HookPrompt(request) => request.model,
            };
            Ok(mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: model.to_owned(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            ))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-concurrent-next.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        const first = next({ ...e, model: 'concurrent-first' });
        const second = next({ ...e, model: 'concurrent-second' });
        const collect = async (stream) => {
          const chunks = [];
          for await (const chunk of stream) chunks.push(chunk);
          return { chunks, result: await stream.result };
        };
        const [a, b] = await Promise.all([collect(first), collect(second)]);
        if (a.result.answer !== 'concurrent-first' || b.result.answer !== 'concurrent-second') {
          throw new Error(`crossed source results: ${a.result.answer}|${b.result.answer}`);
        }
        for (const chunk of b.chunks) yield chunk;
        return b.result;
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "batch-concurrent-next",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(ByModel),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.run_turn("question"),
    )
    .await
    .expect("concurrent next calls finish")
    .unwrap();
    let history = format!("{:?}", orch.session.lock().await.history);
    assert!(history.contains("concurrent-second"), "history: {history}");
    assert!(
        !history.contains("crossed source results"),
        "history: {history}"
    );
}

#[tokio::test]
async fn mod_turn_step_batched_hook_failure_retains_completed_provider_cost() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-failure.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        const below = next(e);
        for await (const _chunk of below) {}
        throw new Error('failure after provider success');
      });
      on('turn.complete', ($, e, next) => {
        $.ui.log(`failed-usage:${e.reason}:${e.usage?.input_tokens}:${e.usage?.output_tokens}`);
        return next(e);
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("batch-failure", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let mut physical = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "provider response".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    physical.usage.counts_mut().input_tokens = 11;
    physical.usage.counts_mut().output_tokens = 7;
    let api = Arc::new(MockApiClient::new(vec![physical]));
    let output = Arc::new(MockOutputStream::new());
    let base = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let session_id = base.session.lock().await.session_id;
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::channel(8);
    let tracker = Arc::new(cost::CostTracker::new(
        session_id,
        Arc::new(cost::PricingCatalog::builtin_reference()),
        persist_tx,
    ));
    let orch = base.with_cost_tracker(tracker.clone());
    let _ = orch.run_turn("question").await;
    assert_eq!(api.captured_models().await.len(), 1);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tracker.drain_owned_settlements(),
    )
    .await
    .expect("completed provider request must finish settlement")
    .unwrap();
    assert_eq!(tracker.snapshot().await.cost_revision, 1);
    let ledger = orch.model_runtime.prompt_cache_ledger.lock().await;
    assert_eq!(ledger.ledger.summary(u64::MAX).requests, 1);
    drop(ledger);
    assert_eq!(
        orch.model_runtime
            .api_calls_recorded
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let logs: Vec<_> = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            lingxi_core::host::OutputEvent::ModLog { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(
        logs.iter().any(|text| text == "failed-usage:error:11:7"),
        "logs: {logs:?}"
    );
}

#[tokio::test]
async fn mod_turn_step_cancel_after_first_next_keeps_physical_usage() {
    struct FirstThenPending {
        calls: std::sync::atomic::AtomicUsize,
        second_started: Arc<tokio::sync::Notify>,
        first: llm_runtime::HistoryResponse,
    }
    #[async_trait::async_trait]
    impl OrchestratorApiClient for FirstThenPending {
        async fn messages_create(
            &self,
            _request: crate::OrchestratorApiRequest,
        ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Ok(self.first.clone());
            }
            self.second_started.notify_one();
            std::future::pending().await
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("batch-cancel-after-first.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        const first = next(e);
        for await (const _chunk of first) {}
        const second = next(e);
        for await (const chunk of second) yield chunk;
        return await second.result;
      });
      on('turn.complete', ($, e, next) => {
        $.ui.log(`cancelled-usage:${e.reason}:${e.usage?.input_tokens}:${e.usage?.output_tokens}`);
        return next(e);
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "batch-cancel-after-first",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let mut first = mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "paid but hidden".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    first.usage.counts_mut().input_tokens = 11;
    first.usage.counts_mut().output_tokens = 7;
    let second_started = Arc::new(tokio::sync::Notify::new());
    let api = Arc::new(FirstThenPending {
        calls: std::sync::atomic::AtomicUsize::new(0),
        second_started: second_started.clone(),
        first,
    });
    let output = Arc::new(MockOutputStream::new());
    let base = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let session_id = base.session.lock().await.session_id;
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::channel(8);
    let tracker = Arc::new(cost::CostTracker::new(
        session_id,
        Arc::new(cost::PricingCatalog::builtin_reference()),
        persist_tx,
    ));
    let orch = Arc::new(base.with_cost_tracker(tracker.clone()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move { orch.run_turn_with_cancel("question", cancel).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        second_started.notified(),
    )
    .await
    .expect("Mod opens its second physical request");
    cancel.cancel();
    assert!(matches!(
        task.await.unwrap().unwrap(),
        TurnOutcome::Cancelled
    ));
    assert_eq!(tracker.snapshot().await.cost_revision, 1);
    assert_eq!(
        orch.model_runtime
            .prompt_cache_ledger
            .lock()
            .await
            .ledger
            .summary(u64::MAX)
            .requests,
        1
    );
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { text, .. }
            if text == "cancelled-usage:aborted:11:7"
    )));
}

#[tokio::test]
async fn mod_turn_complete_reports_cancellation_before_any_response() {
    struct PendingApi {
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl OrchestratorApiClient for PendingApi {
        async fn messages_create(
            &self,
            _request: crate::OrchestratorApiRequest,
        ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
            self.started.notify_one();
            std::future::pending().await
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("turn-cancel.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.complete', ($, e, next) => {
            $.ui.log(`${e.reason}:${e.isAborted}:${e.answer}:${e.usage === undefined}`, { to: 'transcript' });
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("cancel-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let started = Arc::new(tokio::sync::Notify::new());
    let output = Arc::new(MockOutputStream::new());
    let orch = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(PendingApi {
                started: started.clone(),
            }),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move { orch.run_turn_with_cancel("question", cancel).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
        .await
        .unwrap();
    cancel.cancel();
    assert!(matches!(
        task.await.unwrap().unwrap(),
        TurnOutcome::Cancelled
    ));
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { plugin, text }
            if plugin == "cancel-mod" && text == "aborted:true::true"
    )));
}

#[tokio::test]
async fn mod_turn_step_cancel_during_first_request_ends_worker_stream() {
    struct PendingApi {
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl OrchestratorApiClient for PendingApi {
        async fn messages_create(
            &self,
            _request: crate::OrchestratorApiRequest,
        ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
            self.started.notify_one();
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("first-request-cancel.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('turn.step', async function* ($, e, next) {
        const below = next(e);
        for await (const chunk of below) yield chunk;
        return await below.result;
      });
      on('turn.complete', ($, e, next) => {
        $.ui.log(`first-cancel:${e.reason}:${e.isAborted}:${e.usage === undefined}`);
        return next(e);
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "first-request-cancel",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let started = Arc::new(tokio::sync::Notify::new());
    let output = Arc::new(MockOutputStream::new());
    let orch = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(PendingApi {
                started: started.clone(),
            }),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn({
        let orch = orch.clone();
        let cancel = cancel.clone();
        async move { orch.run_turn_with_cancel("question", cancel).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
        .await
        .expect("first wrapped provider request starts");
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("cancelable Mod turn finishes")
            .unwrap()
            .unwrap(),
        TurnOutcome::Cancelled
    ));
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { text, .. }
            if text == "first-cancel:aborted:true:true"
    )));
}

#[tokio::test]
async fn mod_turn_complete_sums_response_usage_and_keeps_last_model() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("turn-usage.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('turn.complete', ($, e, next) => {
            const u = e.usage;
            $.ui.log(`${u.input_tokens}:${u.output_tokens}:${u.cache_read_input_tokens}:${u.cache_creation_input_tokens}:${u.model}:${e.answer}`, { to: 'transcript' });
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("usage-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let usage = |input, output, read, write| llm_runtime::ExecutionUsage {
        report: llm_runtime::UsageReport::measured(
            llm_runtime::Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: read,
                cache_write_tokens: write,
                ..Default::default()
            },
            llm_runtime::services::sdk::protocol::UsageState::Complete,
        ),
        ..Default::default()
    };
    orch.fire_mod_turn_start("question", "turn-usage").await;
    let first = ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "first".into(),
            citations: None,
        }],
        stop_reason: Some("tool_use".into()),
    };
    orch.record_mod_turn_response(
        &first,
        Some(&usage(4, 2, 1, 3)),
        "model-a",
        Some("tool_use"),
        None,
    );
    let last = ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "last".into(),
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
    };
    orch.record_mod_turn_response(
        &last,
        Some(&usage(6, 3, 2, 4)),
        "model-b",
        Some("end_turn"),
        None,
    );
    orch.fire_mod_turn_complete(false, false).await;
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { plugin, text }
            if plugin == "usage-mod" && text == "10:5:3:7:model-b:last"
    )));
}

fn runtime_message(body: &str) -> ConversationMessage {
    ConversationMessage::user_meta(MessageId::new(), body.to_string())
}

fn mobile_environment_with_runtime(
    tool_runtime: lingxi_core::host::MobileToolRuntime,
    cwd: Option<&str>,
) -> lingxi_core::host::MobileRuntimeEnvironment {
    lingxi_core::host::MobileRuntimeEnvironment::new(
        lingxi_core::host::MobileHostEnvironment::new(
            lingxi_core::host::MobileHostOs::Ios,
            Some("19.0".into()),
            lingxi_core::host::MobileDeviceClass::Phone,
            lingxi_core::host::MobileExecutionTarget::PhysicalDevice,
            lingxi_core::host::MobileLaunchMode::Interactive,
        ),
        tool_runtime,
        cwd.map(str::to_string),
        Some("/bin/sh".into()),
        Some("Mobile Linux sh".into()),
        lingxi_core::host::MobileNetworkPolicy::PermissionMediated,
        lingxi_core::host::MobileLifecyclePolicy::IosFiniteBackgroundAssertion,
    )
}

fn mobile_environment(cwd: &str) -> lingxi_core::host::MobileRuntimeEnvironment {
    mobile_environment_with_runtime(
        lingxi_core::host::MobileToolRuntime::MobileLinuxGuest,
        Some(cwd),
    )
}

#[tokio::test]
async fn mod_session_version_reports_the_running_engine_build() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let version = hooks::mods::ModSessionContext::version(&orch)
        .await
        .unwrap();
    assert_eq!(
        version,
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "base": env!("CARGO_PKG_VERSION"),
        })
    );
}

#[tokio::test]
async fn prompt_submit_mod_rewrites_before_history_and_attaches_model_context() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("prompt-submit.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.submit', ($, e, next) => {
            if (e.origin.kind !== 'composer') return { drop: 'wrong origin' };
            if (e.text === 'drop') return { drop: 'refused' };
            return next({ ...e, text: 'rewritten', context: ['private note'] });
          });
        }
    "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("submit", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "ok".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    crate::mod_prompt_origin::with_origin(
        serde_json::json!({"kind":"composer"}),
        orch.run_turn("original"),
    )
    .await
    .unwrap();
    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].iter().any(|message| text(message) == "rewritten"));
    assert!(calls[0].iter().any(|message| {
        text(message).contains("prompt.submit hook additional context: private note")
    }));
    assert!(!calls[0].iter().any(|message| text(message) == "original"));

    crate::mod_prompt_origin::with_origin(
        serde_json::json!({"kind":"composer"}),
        orch.run_turn("drop"),
    )
    .await
    .unwrap();
    assert_eq!(
        api.captured_msgs().await.len(),
        1,
        "dropped prompt never calls model"
    );
    assert!(
        !orch
            .session
            .lock()
            .await
            .history
            .iter()
            .any(|message| text(message) == "drop")
    );
}

#[tokio::test]
async fn prompt_submit_mod_screens_direct_streaming_image_input() {
    use crate::test_support_stream::{
        MockStreamingApiClient, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, text_delta,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("image-submit.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.submit', ($, e, next) => {
            if (e.origin.kind !== 'composer' || e.attachments?.[0]?.mediaType !== 'image/png') {
              return { drop: 'image metadata missing' };
            }
            return next({ ...e, text: 'rewritten image prompt' });
          });
        }
    "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("image-submit", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
        message_start("msg_image", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "ok"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );
    crate::mod_prompt_origin::with_origin(
        serde_json::json!({"kind":"composer"}),
        orch.run_turn_streaming_with_origin(
            "original image prompt",
            vec![lingxi_core::types::ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            }],
            tokio_util::sync::CancellationToken::new(),
            None,
            true,
        ),
    )
    .await
    .unwrap();
    let calls = api.captured_calls().await;
    assert_eq!(calls.len(), 1);
    let messages = &calls[0].messages;
    assert!(
        messages
            .iter()
            .any(|message| text(message) == "rewritten image prompt")
    );
    assert!(
        !messages
            .iter()
            .any(|message| text(message) == "original image prompt")
    );
    assert!(messages.iter().any(|message| matches!(message,
        ConversationMessage::User { content, .. }
        if content.iter().any(|block| matches!(block, ContentBlock::Image { .. }))
    )));
}

#[tokio::test]
async fn prompt_section_mod_rewrites_and_removes_named_system_sections() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("sections.js");
    std::fs::write(
        &module,
        r#"
        let runs = 0;
        export function register(on) {
          on('prompt.section', { name: 'pronouns' }, async ($, e, next) => {
            runs++;
            if (runs === 1) $.ui.invalidate('prompt.section');
            return next({ ...e, text: e.text + '\nMOD PRONOUNS ' + runs });
          });
          on('prompt.section', { name: 'context_management' }, () => ({ text: null }));
          on('prompt.section', { name: 'communication' }, ($, e, next) =>
            next({ ...e, text: e.text + '\nMOD COMMUNICATION' }));
          on('prompt.section', { name: 'brief' }, ($, e) =>
            ({ text: e.text === null ? 'MOD BRIEF' : 'WRONG BRIEF INPUT' }));
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("section-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let first_cwd = dir.path().join("first");
    let second_cwd = dir.path().join("second");
    std::fs::create_dir_all(&first_cwd).unwrap();
    std::fs::create_dir_all(&second_cwd).unwrap();
    let session_cwd = tool_api::SessionCwd::new(first_cwd.clone(), Vec::new());
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
        .with_session_cwd(session_cwd.clone());
    let prompt = orch.build_system_prompt().await;
    assert!(prompt.contains("MOD PRONOUNS 1"));
    assert!(prompt.contains("MOD BRIEF"));
    assert!(!prompt.contains("# Context management"));
    assert!(prompt.starts_with(&format!("You are {}", branding::PRODUCT_NAME)));
    {
        let cache = orch.prompt_runtime.mod_prompt_sections.lock().await;
        assert!(cache.generation > 0, "invalidate was not delivered");
        assert!(
            !cache
                .answers
                .contains_key(&"pronouns".encode_utf16().collect::<Vec<_>>()),
            "invalidated section was cached again"
        );
    }
    let refreshed = orch.build_system_prompt().await;
    assert!(refreshed.contains("MOD PRONOUNS 2"));
    let cached = orch.build_system_prompt().await;
    assert!(cached.contains("MOD PRONOUNS 2"));
    assert!(!cached.contains("MOD PRONOUNS 3"));
    session_cwd.swap(second_cwd.clone(), Vec::new());
    let moved = orch.build_system_prompt().await;
    assert!(moved.contains("MOD PRONOUNS 3"));
    assert!(moved.contains(&format!(
        "Primary working directory: {}",
        second_cwd.canonicalize().unwrap().display()
    )));
    orch.session.lock().await.model = "claude-opus-5".into();
    let lean = orch.build_system_prompt().await;
    assert!(lean.contains("MOD COMMUNICATION"));
    assert!(lean.contains("# Harness"));
    host.unload("section-mod").await.unwrap();
    let unloaded = orch.build_system_prompt().await;
    assert!(!unloaded.contains("MOD PRONOUNS"));
    assert!(!unloaded.contains("MOD BRIEF"));
    assert!(unloaded.contains("# Context management"));
}

#[tokio::test]
async fn prompt_sections_resolve_concurrently_and_keep_slot_order() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("parallel-sections.js");
    std::fs::write(
        &module,
        r#"
        let pronounsStarted = false;
        export function register(on) {
          on('prompt.section', { name: 'communication' }, async ($, e, next) => {
            await $.clock.sleep(150);
            const answer = await next(e);
            return { text: answer.text + '\n' + (pronounsStarted ? 'CONCURRENT SECTIONS' : 'SERIAL SECTIONS') };
          });
          on('prompt.section', { name: 'pronouns' }, async ($, e, next) => {
            pronounsStarted = true;
            const answer = await next(e);
            return { text: answer.text + '\nPRONOUNS PROBE' };
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "parallel-sections",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let prompt = orch.build_system_prompt().await;
    let communication = prompt.find("CONCURRENT SECTIONS").expect("parallel hooks");
    let pronouns = prompt.find("PRONOUNS PROBE").expect("pronouns rewrite");
    assert!(communication < pronouns, "section output order changed");
}

#[tokio::test]
async fn prompt_compose_mod_can_reorder_and_replace_system_sections() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("compose.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.compose', async ($, e, next) => {
            const detached = await $.prompt.compose();
            if (!detached.sections.some(section => section.id === 'context_management')) {
              throw new Error('detached composition omitted context management');
            }
            const answer = await next(e);
            const lean = e.traits.includes('lean');
            const baseIds = answer.sections.some(section => section.id === 'communication')
              && !answer.sections.some(section => section.id.startsWith('communication:'));
            return { sections: [
              ...answer.sections.filter(section => section.id !== 'context_management'),
              { id: 'compose-mod:policy',
                text: lean ? (baseIds ? 'MOD COMPOSE LEAN BASE' : 'MOD COMPOSE WRONG')
                  : 'MOD COMPOSE POLICY', scope: 'session' },
            ] };
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("compose-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    orch.session.lock().await.model = "claude-opus-4-7".into();
    let prompt = orch.build_system_prompt().await;
    assert!(prompt.contains("MOD COMPOSE POLICY"));
    assert!(!prompt.contains("# Context management"));
    assert!(prompt.ends_with("MOD COMPOSE POLICY"));
    let provider_prompt = orch.provider_system_prompt().await.display_text();
    assert!(
        !provider_prompt
            .contains(lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY)
    );
    assert_eq!(provider_prompt, prompt);
    orch.session.lock().await.model = "claude-opus-5".into();
    let lean = orch.build_system_prompt().await;
    assert!(lean.ends_with("MOD COMPOSE LEAN BASE"));
}

#[tokio::test]
async fn prompt_compose_scopes_reach_the_main_request_without_leaking_into_preview() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("scoped-compose.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.compose', () => ({ sections: [
            { id: 'shared', text: 'SHARED MOD TEXT', scope: 'shared' },
            { id: 'session', text: 'SESSION MOD TEXT', scope: 'session' },
          ] }));
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("scoped-compose", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "ok".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    orch.run_turn("hi").await.unwrap();
    let sent = api.captured_systems().await;
    let sent = sent[0].as_deref().unwrap();
    let marker = lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY;
    assert!(sent.starts_with("SHARED MOD TEXT\n\nSESSION MOD TEXT"));
    assert!(!sent.contains(marker));
    let preview = orch.assemble_system_prompt_preview().await;
    assert!(!preview.contains(marker));
    assert!(preview.starts_with("SHARED MOD TEXT\n\nSESSION MOD TEXT"));
}

#[tokio::test]
async fn prompt_compose_next_can_recompute_sdk_sections() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("compose-facts.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.compose', async ($, e, next) => {
            const answer = await next({ ...e, traits: [...e.traits, 'sdk-preset'] });
            return { sections: [
              ...answer.sections,
              { id: 'compose-facts:marker', text: 'RECOMPOSED SDK', scope: 'session' },
            ] };
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("compose-facts", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let prompt = orch.build_system_prompt().await;
    assert!(prompt.contains("RECOMPOSED SDK"));
    assert!(!prompt.contains("Primary working directory:"));
    assert!(prompt.contains("# Context management"));
}

#[tokio::test]
async fn prompt_compose_api_returns_detached_hooked_sections() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("compose-api.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.section', { name: 'brief' }, ($, e, next) =>
            ({ text: `${next.origin.plugin}:${next.origin.tier}` }));
          on('prompt.compose', async ($, e, next) => {
            const answer = await next(e);
            return { sections: [...answer.sections,
              { id: 'compose-api:marker', text: 'COMPOSE API MARKER', scope: 'session' }] };
          });
          on('prompt.context', async ($, e, next) => {
            const answer = await $.prompt.compose({ model: 'claude-opus-5', traits: ['sdk-preset'] });
            const ids = answer.sections.map(section => section.id);
            const ok = ids.includes('lean_body') && ids.includes('compose-api:marker')
              && !ids.includes('env_info_simple') && ids.includes('context_management')
              && answer.sections.some(section => section.id === 'brief'
                && section.text === 'compose-api:user');
            return next({ ...e, blocks: [...e.blocks,
              { name: 'composeProof', text: ok ? 'COMPOSE API OK' : 'COMPOSE API WRONG' }] });
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("compose-api", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let body = text(
        &orch
            .additional_context_message()
            .await
            .expect("context message"),
    );
    assert!(body.contains("# composeProof\nCOMPOSE API OK"));
    assert!(
        orch.prompt_runtime
            .mod_prompt_sections
            .lock()
            .await
            .answers
            .is_empty()
    );
}

#[tokio::test]
async fn prompt_compose_facts_use_mobile_surface_for_mobile_sessions() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_mobile_runtime_environment(mobile_environment("/workspace/mobile"));
    let facts = orch
        .mod_prompt_compose_facts(hooks::mods::ModUtf16ValueProjection::plain(
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(facts.value["surfaces"], serde_json::json!(["mobile"]));
    assert_eq!(
        <ConversationOrchestrator as hooks::mods::ModSessionContext>::surfaces(&orch),
        ["mobile"]
    );
}

#[tokio::test]
async fn prompt_compose_facts_use_attached_desktop_surface() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    orch.config.mod_render_surface = Some(crate::config::ModRenderSurface::Desktop);
    let facts = orch
        .mod_prompt_compose_facts(hooks::mods::ModUtf16ValueProjection::plain(
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(facts.value["surfaces"], serde_json::json!(["desktop"]));
    assert_eq!(
        <ConversationOrchestrator as hooks::mods::ModSessionContext>::surfaces(&orch),
        ["desktop"]
    );
}

#[tokio::test]
async fn mod_ui_selection_reads_live_fullscreen_text_and_clears() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("selection.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('tool.call', async ($) => {
            const selection = await $.ui.selection();
            return { result: { selection, absent: selection === undefined } };
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("selection", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    lingxi_core::host::OrchestratorHandle::set_mod_ui_selection(
        &orch,
        Some(lingxi_core::host::ModUiSelection {
            text: "selected text".into(),
            request_id: Some("toolu_123".into()),
        }),
    );
    assert_eq!(
        <ConversationOrchestrator as hooks::mods::ModSessionContext>::ui_selection(&orch)
            .await
            .unwrap(),
        Some(serde_json::json!({"text":"selected text","requestId":"toolu_123"}))
    );
    let result = host
        .dispatch_with_log_at_session(
            "tool.call",
            serde_json::json!({"tool":"Selection"}),
            &orch,
            |_| async { panic!("Mod answers tool.call") },
            |_, _| async {},
        )
        .await
        .unwrap();
    assert_eq!(
        result["result"]["selection"],
        serde_json::json!({"text":"selected text","requestId":"toolu_123"})
    );
    assert_eq!(result["result"]["absent"], false);
    lingxi_core::host::OrchestratorHandle::set_mod_ui_selection(
        &orch,
        Some(lingxi_core::host::ModUiSelection {
            text: "unattributed".into(),
            request_id: None,
        }),
    );
    assert_eq!(
        <ConversationOrchestrator as hooks::mods::ModSessionContext>::ui_selection(&orch)
            .await
            .unwrap(),
        Some(serde_json::json!({"text":"unattributed"}))
    );
    lingxi_core::host::OrchestratorHandle::set_mod_ui_selection(&orch, None);
    assert_eq!(
        <ConversationOrchestrator as hooks::mods::ModSessionContext>::ui_selection(&orch)
            .await
            .unwrap(),
        None
    );
    let cleared = host
        .dispatch_with_log_at_session(
            "tool.call",
            serde_json::json!({"tool":"Selection"}),
            &orch,
            |_| async { panic!("Mod answers tool.call") },
            |_, _| async {},
        )
        .await
        .unwrap();
    assert_eq!(cleared["result"]["absent"], true);
    assert!(cleared["result"].get("selection").is_none());
}

#[tokio::test]
async fn mod_session_surface_is_empty_for_headless_session() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    orch.config.interactive_session = false;
    assert!(
        <ConversationOrchestrator as hooks::mods::ModSessionContext>::surfaces(&orch).is_empty()
    );
}

#[tokio::test]
async fn prompt_context_is_cached_until_mod_invalidate_or_gv_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("context-cache.js");
    std::fs::write(
        &module,
        r#"
        let runs = 0;
        export function register(on) {
          on('prompt.context', async ($, e, next) => {
            const answer = await next(e);
            return { ...answer, blocks: [...answer.blocks,
              { name: 'run', text: String(++runs) }] };
          });
          on('tool.call', async ($) => {
            $.ui.invalidate('prompt.context');
            return { result: 'invalidated' };
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("context-cache", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let first = text(&orch.additional_context_message().await.unwrap());
    let second = text(&orch.additional_context_message().await.unwrap());
    assert!(first.contains("# run\n1"));
    assert_eq!(first, second);

    host.dispatch_with_log_at_session(
        "tool.call",
        serde_json::json!({"tool":"Bash"}),
        &orch,
        |_| async { panic!("Mod handles tool.call") },
        |_, _| async {},
    )
    .await
    .unwrap();
    let after_invalidate = text(&orch.additional_context_message().await.unwrap());
    assert!(after_invalidate.contains("# run\n2"));

    let next_cwd = dir.path().join("next");
    std::fs::create_dir(&next_cwd).unwrap();
    orch.session_cwd.change_cwd(next_cwd);
    let after_cwd_change = text(&orch.additional_context_message().await.unwrap());
    assert!(after_cwd_change.contains("# run\n2"));
    orch.invalidate_instruction_context(
        lingxi_core::host::instructions::InstructionRefreshReason::SettingsSync,
    );
    let after_context_refresh = text(&orch.additional_context_message().await.unwrap());
    assert!(after_context_refresh.contains("# run\n3"));
}

#[tokio::test]
async fn prompt_context_mod_instruction_files_reach_the_model_reminder() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("context.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.context', ($, e, next) => next({
            ...e,
            instructionFiles: [
              ...e.instructionFiles,
              { path: '/project/AGENTS.md', kind: 'project', content: 'FOLLOW AGENT RULES' },
            ],
          }));
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("context-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with_rendering(
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        None,
        lingxi_core::host::instructions::InstructionRendering::Announced,
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let body = text(
        &orch
            .additional_context_message()
            .await
            .expect("context message"),
    );
    assert!(body.contains("# instructions\nCodebase and user instructions are shown below."));
    assert!(body.contains("Contents of /project/AGENTS.md (project instructions, checked into the codebase):\n\nFOLLOW AGENT RULES"));
    assert!(body.contains("# currentDate\nToday's date is "));
    let announcements = orch.context_announcement_messages().await;
    assert!(
        announcements
            .iter()
            .map(text)
            .collect::<String>()
            .contains("FOLLOW AGENT RULES")
    );
    let context = orch.instruction_context_snapshot().await;
    assert!(context.eager_instructions.as_ref().is_some_and(|files| {
        files
            .iter()
            .any(|file| file.path == "/project/AGENTS.md" && file.content == "FOLLOW AGENT RULES")
    }));
}

#[tokio::test]
async fn prompt_context_repeated_names_keep_first_position_and_last_value() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("context-duplicate.js");
    std::fs::write(
        &module,
        r#"
      export function register(on) {
        on('prompt.context', async ($, e, next) => {
          const answer = await next(e);
          return { ...answer, blocks: [...answer.blocks,
            { name: 'repeat', text: 'first' },
            { name: 'after', text: 'middle' },
            { name: 'repeat', text: 'last' }] };
        });
      }
    "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "context-duplicate",
        dir.path(),
        &module,
        serde_json::json!({}),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with_rendering(
        Arc::new(StaticMemoryProvider::empty()),
        None,
        lingxi_core::host::instructions::InstructionRendering::Announced,
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let body = text(&orch.additional_context_message().await.unwrap());
    assert_eq!(body.matches("# repeat\n").count(), 1);
    assert!(!body.contains("# repeat\nfirst"));
    assert!(body.find("# repeat\nlast").unwrap() < body.find("# after\nmiddle").unwrap());
    let announcements = orch.context_announcement_messages().await;
    let announced = announcements.iter().map(text).collect::<String>();
    assert!(announced.contains("# repeat\nlast"));
    assert!(!announced.contains("# repeat\nfirst"));
    assert!(announced.find("# repeat\nlast").unwrap() < announced.find("# after\nmiddle").unwrap());
}

#[tokio::test]
async fn prompt_context_mod_sees_import_source_and_can_remove_all_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("context.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.context', ($, e) => {
            const file = e.instructionFiles[0];
            if (file.content !== '  imported body\n' || file.parent !== '/project/LINGXI.md') {
              throw new Error('import provenance or parsed content was lost');
            }
            return { blocks: [] };
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("context-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let memory = Arc::new(StaticMemoryProvider::with_files(vec![MemoryFile {
        path: std::path::PathBuf::from("/project/import.md"),
        parent: Some(std::path::PathBuf::from("/project/LINGXI.md")),
        source_content: Some("  imported body\n".into()),
        body: "imported body".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "  imported body\n".into(),
        content_differs_from_disk: false,
    }]));
    let orch =
        orch_with(memory, None).with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    assert!(orch.additional_context_message().await.is_none());
}

#[tokio::test]
async fn all_three_keys_byte_exact_order_and_wrapper() {
    // instructions + userEmail present; currentDate always present. Insertion
    // order (claude-code `pS`): instructions, userEmail, currentDate.
    let mem = Arc::new(StaticMemoryProvider::with_files(vec![MemoryFile {
        parent: None,
        source_content: None,
        path: std::path::PathBuf::from("/proj/LINGXI.md"),
        body: "MD BODY".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "MD BODY".into(),
        content_differs_from_disk: false,
    }]));
    let orch = orch_with(mem, Some("u@example.com"));
    let msg = orch.additional_context_message().await.expect("present");
    // It is a META user message (claude-code `isMeta:!0`).
    assert!(msg.is_meta(), "additionalContext must be isMeta");
    let body = text(&msg);

    // Exact wrapper: opens with the header line, closes with the IMPORTANT
    // line indented by 6 spaces + the closing tag + trailing LF.
    assert!(body.starts_with(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n"
    ));
    assert!(body.ends_with(&format!("\n\n      {} attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.\n</system-reminder>\n", branding::PRODUCT_NAME)));

    // Keys in order, each `# key\nvalue`, joined by `\n`.
    let i_md = body.find("# instructions\n").expect("instructions key");
    let i_email = body.find("# userEmail\n").expect("userEmail key");
    let i_date = body.find("# currentDate\n").expect("currentDate key");
    assert!(
        i_md < i_email && i_email < i_date,
        "key order instructions<userEmail<currentDate"
    );

    // instructions value = the assembled memory block (preamble + Contents).
    assert!(body.contains("# instructions\nCodebase and user instructions are shown below."));
    assert!(body.contains("Contents of /proj/LINGXI.md"));
    assert!(body.contains("MD BODY"));
    // userEmail value.
    assert!(body.contains("# userEmail\nThe user's email address is u@example.com."));
    // currentDate value (ISO local date).
    let today = crate::prompt::env_meta::current_date_string();
    assert!(body.contains(&format!("# currentDate\nToday's date is {today}.")));
}

#[tokio::test]
async fn omits_lingxi_md_and_email_when_absent_keeps_date() {
    // Empty memory + no email → only `# currentDate` remains.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let msg = orch
        .additional_context_message()
        .await
        .expect("date always present");
    let body = text(&msg);
    assert!(!body.contains("# instructions"));
    assert!(!body.contains("# userEmail"));
    assert!(body.contains("# currentDate\nToday's date is "));
    // The body between the header and the IMPORTANT line is exactly the one
    // currentDate entry (no stray blank lines from empty entries).
    let today = crate::prompt::env_meta::current_date_string();
    let expected = format!(
        "<system-reminder>\n\
As you answer the user's questions, you can use the following context:\n\
# currentDate\nToday's date is {today}.\n\
\n      {} attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.\n\
</system-reminder>\n",
        branding::PRODUCT_NAME
    );
    assert_eq!(body, expected, "single-key wrapper byte-lock");
}

#[tokio::test]
async fn native_email_truthiness_omits_empty_and_preserves_raw_whitespace() {
    // Native yLn uses `...D&&{userEmail:...}` without trimming D. The actual
    // extracted helper omits "" and preserves both whitespace-only and raw
    // surrounding whitespace in the complete privacy text.
    for email in ["", "   ", " \t raw@example.test \n"] {
        let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), Some(email));
        let body = text(&orch.additional_context_message().await.expect("date"));
        if email.is_empty() {
            assert!(!body.contains("# userEmail"));
        } else {
            let expected = format!(
                "# userEmail\nThe user's email address is {email}. Use it only to identify the user, such as for authorship, attribution, or filtering their own work. Never send it to an unrelated service, such as in a request header, URL, or payload, unless the user explicitly asks."
            );
            assert!(
                body.contains(&expected),
                "email bytes must remain unchanged"
            );
        }
    }
}

#[tokio::test]
async fn runtime_message_is_prepended_before_additional_context() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let runtime = "<system-reminder>\nMobile runtime environment (version 1)\n</system-reminder>";
    orch.mobile_runtime_environment_message = Some(runtime_message(runtime));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));
    orch.mobile_workspace_cwd_resolver = Some(Arc::new(|_| Some("/workspace/a".into())));
    let original = ConversationMessage::user(MessageId::new(), "hello".into());
    let mut messages = vec![original.clone()];

    let context = crate::conversation::context_announcements_impl::PreparedContextAnnouncements {
        inline_context: orch.additional_context_message().await,
        ..Default::default()
    };
    orch.prepend_leading_context(&mut messages, &context).await;

    assert_eq!(text(&messages[0]), runtime);
    assert!(text(&messages[1]).contains("Guest workspace: /workspace/a"));
    assert!(text(&messages[2]).contains("# currentDate\nToday's date is "));
    assert_eq!(messages[3], original);
    assert_eq!(
        orch.mobile_runtime_environment_preview().await.as_deref(),
        Some(runtime)
    );
}

#[tokio::test]
async fn unresolved_native_workspace_path_falls_back_to_guest_coordinate() {
    let host_cwd = std::path::PathBuf::from("/tmp/native-host-worktree");
    let session_cwd = tool_api::SessionCwd::new(host_cwd.clone(), vec![host_cwd]);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_session_cwd(session_cwd)
        .with_mobile_runtime_environment(mobile_environment("/workspace/a"))
        .with_mobile_workspace_cwd_resolver(Arc::new(|_| None));
    let mut messages = Vec::new();

    let context = crate::conversation::context_announcements_impl::PreparedContextAnnouncements {
        inline_context: orch.additional_context_message().await,
        ..Default::default()
    };
    orch.prepend_leading_context(&mut messages, &context).await;

    assert!(text(&messages[1]).contains("Guest workspace: /workspace/a"));
    assert!(!text(&messages[1]).contains("native-host-worktree"));
}

#[test]
fn scheduled_mobile_runtime_uses_headless_prompt_guidance() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    orch.config.interactive_session = true;
    assert!(orch.prompt_is_interactive());

    let mut environment = mobile_environment("/workspace/a");
    environment.host.launch_mode = lingxi_core::host::MobileLaunchMode::ScheduledHeadless;
    orch.mobile_runtime_environment = Some(environment);

    assert!(!orch.prompt_is_interactive());
}

#[tokio::test]
async fn runtime_message_stays_first_when_transient_context_is_reattached() {
    let mut orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let runtime = "<system-reminder>runtime</system-reminder>";
    let deferred = runtime_message("<system-reminder>deferred</system-reminder>");
    let date = runtime_message("<system-reminder>date</system-reminder>");
    let tail = runtime_message("<system-reminder>tail</system-reminder>");
    orch.mobile_runtime_environment_message = Some(runtime_message(runtime));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));
    orch.mobile_workspace_cwd_resolver = Some(Arc::new(|_| Some("/workspace/a".into())));
    let original = ConversationMessage::user(MessageId::new(), "hello".into());
    orch.session.lock().await.history.push(original.clone());
    let prepared = orch
        .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
        .await
        .unwrap();
    let captured = prepared
        .context_announcements
        .inline_context
        .as_ref()
        .unwrap();
    let captured_bytes = serde_json::to_vec(captured).unwrap();
    assert_eq!(text(&prepared.snapshot[0]), runtime);
    assert!(text(&prepared.snapshot[1]).contains("Guest workspace: /workspace/a"));
    assert_eq!(&prepared.snapshot[2], captured);
    assert_eq!(prepared.snapshot[3], original);
    assert!(text(captured).contains("# currentDate\nToday's date is "));

    // The actual query captured its inline frame before retry/compaction.
    // Reattachment must use those bytes, not acquire the changed Gv again.
    orch.config.user_email = Some("unrelated-later-context@example.test".into());
    orch.invalidate_instruction_context(
        lingxi_core::host::instructions::InstructionRefreshReason::AccountChange,
    );
    let mut turn_reminders = prepared.turn_reminders.clone();
    turn_reminders.push(tail.clone());
    let mut guarded_async_hook_reminders = prepared.guarded_async_hook_reminders.clone();
    for after_compaction in [false, true] {
        let mut messages = vec![original.clone()];
        orch.reattach_outgoing_context(
            &mut messages,
            Some(&deferred),
            Some(&date),
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
            &prepared.context_announcements,
            after_compaction,
        )
        .await;

        assert_eq!(messages.len(), 6 + turn_reminders.len());
        assert_eq!(text(&messages[0]), runtime);
        assert!(text(&messages[1]).contains("Guest workspace: /workspace/a"));
        assert_eq!(messages[2], date);
        assert_eq!(messages[3], deferred);
        assert_eq!(&messages[4], captured);
        assert_eq!(serde_json::to_vec(&messages[4]).unwrap(), captured_bytes);
        assert!(!text(&messages[4]).contains("unrelated-later-context@example.test"));
        assert_eq!(messages[5], original);
        assert_eq!(&messages[6..], turn_reminders.as_slice());
    }
}

#[tokio::test]
async fn runtime_message_keeps_dynamic_environment_separate() {
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            exclude_dynamic_system_prompt_sections: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.mobile_runtime_environment_message = Some(runtime_message(
        "<system-reminder>\nMobile runtime environment (version 1)\n</system-reminder>",
    ));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));
    orch.mobile_workspace_cwd_resolver = Some(Arc::new(|_| Some("/workspace/a".into())));

    let body = text(&orch.additional_context_message().await.expect("date"));
    assert!(body.contains("# Environment\n"));
    assert!(body.contains("# currentDate\nToday's date is "));
}

#[tokio::test]
async fn non_guest_mobile_runtime_keeps_environment_re_emission_when_excluded() {
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            exclude_dynamic_system_prompt_sections: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.mobile_runtime_environment = Some(mobile_environment_with_runtime(
        lingxi_core::host::MobileToolRuntime::AndroidLegacy,
        None,
    ));

    let body = text(&orch.additional_context_message().await.expect("date"));
    assert!(body.contains("# Environment\n"));
    assert!(body.contains("# currentDate\nToday's date is "));
}

#[tokio::test]
async fn system_prompt_override_stays_verbatim_while_runtime_message_is_sent() {
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "ok".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            system_prompt_override: Some("CUSTOM PROMPT — no assembler".into()),
            context_rendering: lingxi_core::host::instructions::InstructionRendering::Inline,
            ..OrchestratorConfig::default()
        },
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let runtime = "<system-reminder>\nMobile runtime environment (version 1)\n</system-reminder>";
    orch.mobile_runtime_environment_message = Some(runtime_message(runtime));
    orch.mobile_runtime_environment = Some(mobile_environment("/workspace/a"));
    orch.mobile_workspace_cwd_resolver = Some(Arc::new(|_| Some("/workspace/a".into())));

    orch.run_turn("hi").await.expect("turn");

    assert_eq!(
        api.captured_systems().await,
        vec![Some("CUSTOM PROMPT — no assembler".into())]
    );
    let sent = api.captured_msgs().await;
    assert_eq!(text(&sent[0][0]), runtime);
    assert!(text(&sent[0][1]).contains("Guest workspace: /workspace/a"));
    assert!(text(&sent[0][2]).contains("# currentDate\nToday's date is "));
}

// ------------------------------------------------------------------------
// `date_change` (cc `Cop`): mid-session midnight crossing.
// ------------------------------------------------------------------------

/// Rewind the memoized session-start date so the live local date always
/// differs — the "session started yesterday" setup.
fn seed_stale_session_date(
    orch: &ConversationOrchestrator,
    session_id: lingxi_core::types::SessionId,
) {
    let mut state = orch
        .prompt_runtime
        .date_change
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.session_id = Some(session_id);
    state.session_date = "2000-01-01".to_string();
    state.delivered_date = None;
}

fn expected_date_change_body() -> String {
    let today = crate::prompt::env_meta::current_date_string();
    format!(
        "<system-reminder>\nThe date has changed. Today's date is now {today}. \
No need to announce the new date \u{2014} the user's own clock shows it.\n</system-reminder>"
    )
}

#[tokio::test]
async fn additional_context_keeps_the_session_start_date_after_rollover() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = orch.session.lock().await.session_id;
    seed_stale_session_date(&orch, sid);

    let body = text(
        &orch
            .additional_context_message()
            .await
            .expect("date context"),
    );
    assert!(body.contains("# currentDate\nToday's date is 2000-01-01."));
    assert!(
        orch.date_change_reminder_message(sid).is_some(),
        "rollover is announced only by date_change"
    );
}

#[test]
fn date_change_none_when_date_unchanged() {
    // First producer run seeds the session-start memo (`LGe = Vr(wcs)`), so
    // a same-day session NEVER emits — the locked fixtures stay identical.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = lingxi_core::types::SessionId::new();
    assert!(orch.date_change_reminder_message(sid).is_none());
    assert!(orch.date_change_reminder_message(sid).is_none());
}

#[test]
fn date_change_emits_once_after_midnight() {
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = lingxi_core::types::SessionId::new();
    seed_stale_session_date(&orch, sid);
    let msg = orch
        .date_change_reminder_message(sid)
        .expect("date differs from session start");
    // Byte-exact reminder (renderer @238108493) inside the `Ww` wrap.
    assert_eq!(text(&msg), expected_date_change_body());
    // Meta user message (`zr({…, isMeta:!0})`).
    assert!(matches!(
        msg,
        ConversationMessage::User { is_meta: true, .. }
    ));
    // The producer is PURE: without a commit the SAME reminder is still due,
    // so a step that never reaches the model cannot swallow it.
    assert!(orch.date_change_reminder_message(sid).is_some());
    orch.commit_date_change_reminder();
    // Dedupe: once delivered, the following turn (same date) emits nothing.
    assert!(orch.date_change_reminder_message(sid).is_none());
}

#[test]
fn date_change_stays_deduped_after_a_compact_boundary() {
    // Compaction must not reset the session-level reminder. The leading
    // `currentDate` remains the session-start memo and the changed date was
    // already delivered once.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let sid = lingxi_core::types::SessionId::new();
    seed_stale_session_date(&orch, sid);
    assert!(orch.date_change_reminder_message(sid).is_some());
    orch.commit_date_change_reminder();
    assert!(orch.date_change_reminder_message(sid).is_none());
    assert!(orch.date_change_reminder_message(sid).is_none());
}

#[test]
fn date_change_re_seeds_the_session_start_date_on_a_new_session() {
    // `clearSessionCaches` clears BOTH `LGe`'s memo and the emitted date, so
    // a `/clear` (fresh `SessionId`) or in-place resume (adopted id) must
    // NOT fire a reminder into the brand-new conversation.
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None);
    let old = lingxi_core::types::SessionId::new();
    seed_stale_session_date(&orch, old);
    assert!(orch.date_change_reminder_message(old).is_some());

    let fresh = lingxi_core::types::SessionId::new();
    assert!(
        orch.date_change_reminder_message(fresh).is_none(),
        "a new session re-seeds the start date to today"
    );
}

// ------------------------------------------------------------------
// PathAtlas S3 (mobile-linux): the `instructions` memory block must be
// probed at the HOST directory backing the guest session cwd.
//
// `additional_context_message` is the ONLY render path for the memory
// block (`prompt/mod.rs` no longer splices it into the system prompt),
// and on mobile `session_cwd` holds the GUEST path
// (`engine-mobile/src/host.rs`: `model_cwd` comes from
// `workspace_mount.guest_path`). Loading memory at that raw guest path
// means a workspace `LINGXI.md` never reaches the model at all.
// `build_prompt_context` already hops guest→host through
// `prompt_probe_cwd_resolver`; this message must use the same
// coordinate.
// ------------------------------------------------------------------

fn orch_with_provider(
    memory: Arc<dyn crate::prompt::MemoryHierarchyProvider>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory,
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn claude_md_is_probed_at_the_host_dir_behind_a_guest_session_cwd() {
    let host = tempfile::tempdir().expect("tempdir");
    let host_root = host.path().to_path_buf();
    std::fs::write(
        host_root.join("LINGXI.md"),
        "MARKER-host-probed-workspace-contract",
    )
    .expect("seed LINGXI.md");

    // The guest path the model sees. It does NOT exist on the host, which
    // is exactly why probing it directly yields nothing.
    let guest = std::path::PathBuf::from("/workspace/app-pathatlas-s3-probe");

    let resolver_root = host_root.clone();
    let orch = orch_with_provider(Arc::new(crate::prompt::RealMemoryHierarchyProvider))
        .with_session_cwd(tool_api::SessionCwd::new(guest.clone(), Vec::new()))
        .with_prompt_probe_cwd_resolver(Arc::new(move |_path: &std::path::Path| {
            resolver_root.clone()
        }));

    let body = text(&orch.additional_context_message().await.expect("present"));
    assert!(
        body.contains("MARKER-host-probed-workspace-contract"),
        "workspace LINGXI.md must reach the model; body was:\n{body}"
    );
    assert!(
        body.contains(&format!(
            "Contents of {}",
            host_root.join("LINGXI.md").display()
        )),
        "the memory block must name the HOST path; body was:\n{body}"
    );
}

#[tokio::test]
async fn claude_md_without_a_resolver_still_probes_the_session_cwd() {
    // Desktop INERT INVARIANT: no resolver installed ⇒ `probe_cwd == cwd`,
    // so the memory block is byte-identical to before the guest→host hop.
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("LINGXI.md"),
        "MARKER-desktop-session-cwd-probe",
    )
    .expect("seed LINGXI.md");

    let orch =
        orch_with_provider(Arc::new(crate::prompt::RealMemoryHierarchyProvider)).with_session_cwd(
            tool_api::SessionCwd::new(root.path().to_path_buf(), Vec::new()),
        );

    let body = text(&orch.additional_context_message().await.expect("present"));
    assert!(
        body.contains("MARKER-desktop-session-cwd-probe"),
        "no-resolver path must keep probing the session cwd; body was:\n{body}"
    );
}

#[tokio::test]
async fn git_status_uses_the_host_dir_behind_a_guest_session_cwd() {
    let host = tempfile::tempdir().expect("tempdir");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(host.path())
            .status()
            .expect("git is available")
            .success()
    };
    assert!(git(&["init", "-q"]));
    assert!(git(&["config", "user.name", "Prompt Probe"]));
    assert!(git(&[
        "config",
        "user.email",
        "prompt-probe@example.invalid"
    ]));
    assert!(git(&["config", "commit.gpgsign", "false"]));
    std::fs::write(host.path().join("tracked.txt"), "seed").expect("seed file");
    assert!(git(&["add", "tracked.txt"]));
    assert!(git(&["commit", "-q", "-m", "seed"]));

    let guest = std::path::PathBuf::from("/workspace/app-pathatlas-git-probe");
    let resolver_root = host.path().to_path_buf();
    let orch = orch_with_provider(Arc::new(StaticMemoryProvider::empty()))
        .with_session_cwd(tool_api::SessionCwd::new(guest, Vec::new()))
        .with_prompt_probe_cwd_resolver(Arc::new(move |_path: &std::path::Path| {
            resolver_root.clone()
        }));

    let _prompt = orch.build_system_prompt().await;
    let announced = orch.context_announcement_messages().await;
    let announced = announced.iter().map(text).collect::<String>();
    assert!(
        announced.contains("# gitStatus\nThis is the git status at the start of the conversation."),
        "the host-backed gitStatus snapshot must reach the normal attachment; request context was:\n{announced}"
    );
    assert!(announced.contains("Git user: Prompt Probe"));
}

#[tokio::test]
async fn prompt_mod_sections_and_context_keep_exact_utf16_until_provider_projection() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("prompt-utf16.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.compose', async ($, event, next) => {
            const answer = await next(event);
            const text = String.fromCharCode(0xd800, 0x03a9, 0xdc00);
            const sections = [...answer.sections];
            const firstSession = sections.findIndex(section => section.scope === 'session');
            const insertAt = firstSession < 0 ? sections.length : firstSession;
            sections.splice(insertAt, 0, { id: 'utf16_source', text, scope: 'shared' });
            return { sections };
          });
          on('prompt.context', async ($, event, next) => {
            const text = String.fromCharCode(0xd800, 0x03a9, 0xdc00);
            return next({ ...event, blocks: [...event.blocks,
              { name: 'utf16_context', text }] });
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("prompt-utf16", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = orch_with(Arc::new(StaticMemoryProvider::empty()), None)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let expected = [0xd800, 0x03a9, 0xdc00];

    let provider_input = orch.provider_system_prompt().await;
    let lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput::SourceVector {
        elements,
        ..
    } = provider_input
    else {
        panic!("the current prompt composer returns a source vector");
    };
    assert!(
        elements
            .iter()
            .any(|element| element.utf16_code_units() == expected)
    );

    let context = orch
        .additional_context_message()
        .await
        .expect("context block");
    let ConversationMessage::User { content, .. } = context else {
        panic!("additional context is a user message");
    };
    let context_units = content.iter().find_map(|block| match block {
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } if utf16_code_units
            .windows(expected.len())
            .any(|window| window == expected) =>
        {
            Some(utf16_code_units)
        }
        _ => None,
    });
    assert!(
        context_units.is_some(),
        "context output must retain the JS units"
    );
}
