use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use std::sync::Arc;

fn source_text(text: &str) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
        text.to_owned(),
        text.encode_utf16().collect(),
    )
    .unwrap()
}

#[tokio::test]
async fn uds_receive_gate_queues_rewrite_once_and_skips_consumed_delivery() {
    use lingxi_core::host::uds_inbox::PeerReceiveGate;

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("uds-receive.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('session.receive', { origin: 'peer' }, ($, e, next) =>
            e.text === 'muted' ? { consumed: 'muted' }
              : next({ ...e, text: 'rewritten' }));
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "uds-receive",
        dir.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let session_id = lingxi_core::host::OrchestratorHandle::current_session_id(&orch)
        .await
        .as_uuid()
        .to_string();
    let queued = Arc::new(std::sync::Mutex::new(Vec::<
        lingxi_core::types::utf16_json::Utf16JsonProjection,
    >::new()));
    let committed = queued.clone();
    let commit: lingxi_core::host::uds_inbox::PeerQueueCommit = Arc::new(move |text| {
        committed.lock().unwrap().push(text);
        true
    });
    assert!(
        orch.receive(&session_id, &source_text("raw"), commit.clone())
            .await
    );
    assert!(
        orch.receive(&session_id, &source_text("muted"), commit)
            .await
    );
    assert_eq!(*queued.lock().unwrap(), vec![source_text("rewritten")]);
    assert!(orch.session.lock().await.history.is_empty());
}

struct ReceivedMidTurnBatch(
    std::sync::Mutex<Option<Vec<crate::prompt::mid_turn_input::MidTurnInput>>>,
);

#[async_trait::async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for ReceivedMidTurnBatch {
    async fn take_mid_turn_input(
        &self,
    ) -> Option<lingxi_core::types::utf16_json::Utf16JsonProjection> {
        None
    }

    async fn take_mid_turn_batch(
        &self,
    ) -> Option<Vec<crate::prompt::mid_turn_input::MidTurnInput>> {
        self.0.lock().unwrap().take()
    }
}

#[tokio::test]
async fn mid_turn_peer_arrivals_are_screened_before_joining_with_human_input() {
    use crate::prompt::mid_turn_input::MidTurnInput;

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("receive-mid-turn.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('session.receive', { origin: 'peer' }, ($, e, next) =>
            e.text === 'muted' ? { consumed: 'muted' }
              : next({ ...e, text: `received ${e.text}` }));
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "receive-mid-turn",
        dir.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
    .with_mid_turn_input(Arc::new(ReceivedMidTurnBatch(std::sync::Mutex::new(Some(
        vec![
            MidTurnInput {
                text: source_text("hello"),
                origin_kind: Some("peer"),
            },
            MidTurnInput {
                text: source_text("muted"),
                origin_kind: Some("peer"),
            },
            MidTurnInput {
                text: source_text("human"),
                origin_kind: None,
            },
        ],
    )))));

    assert!(orch.drain_mid_turn_input().await);
    let session = orch.session.lock().await;
    let text = session.history.last().unwrap().text_content();
    assert!(text.starts_with(
        "Another Claude session sent a message while you were working:\nreceived hello"
    ));
    assert!(text.contains("The user sent a new message while you were working:\nhuman"));
    assert!(!text.contains("muted"));
}

#[tokio::test]
async fn session_receive_only_persists_the_first_core_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("receive.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('session.receive', { origin: 'peer' }, async ($, e, next) => {
            if (e.text === 'drop') return { consumed: 'muted' };
            if (e.text === 'pretend') return { text: 'no next' };
            if (e.text === 'twice') {
              await next({ ...e, text: 'first' });
              await next({ ...e, text: 'second' });
              return { text: 'outer answer' };
            }
            return next({ ...e, text: 'rewritten' });
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "receive",
        dir.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));

    assert!(
        orch.screen_mod_session_receive_source(&source_text("raw"), "peer")
            .await
    );
    assert!(
        !orch
            .screen_mod_session_receive_source(&source_text("drop"), "peer")
            .await
    );
    assert!(
        !orch
            .screen_mod_session_receive_source(&source_text("pretend"), "peer")
            .await
    );
    assert!(
        orch.screen_mod_session_receive_source(&source_text("twice"), "peer")
            .await
    );
    let session = orch.session.lock().await;
    let deliveries = session
        .history
        .iter()
        .filter(|message| message.is_meta())
        .map(ConversationMessage::text_content)
        .collect::<Vec<_>>();
    assert_eq!(deliveries, ["rewritten".to_string(), "first".to_string()]);
    drop(session);

    assert!(
        orch.inject_accepted_peer_deliveries(vec![
            lingxi_core::host::live_sessions::AcceptedPeerDelivery {
                text: source_text("already screened"),
                origin_kind: "peer",
                already_screened: true,
            },
            lingxi_core::host::live_sessions::AcceptedPeerDelivery {
                text: lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                    "�".into(),
                    vec![0xd800],
                )
                .unwrap(),
                origin_kind: "peer",
                already_screened: true,
            },
        ])
        .await
    );
    assert_eq!(
        orch.session
            .lock()
            .await
            .history
            .last()
            .unwrap()
            .text_content(),
        "�"
    );
    assert!(matches!(
        &orch.session.lock().await.history.last().unwrap(),
        ConversationMessage::User { content, .. }
            if matches!(&content[0], lingxi_core::types::ContentBlock::TextJsUtf16 { utf16_code_units, .. } if utf16_code_units == &vec![0xd800])
    ));
}

#[tokio::test]
async fn external_prompts_are_received_before_prompt_submit_and_model_request() {
    use crate::test_support_stream::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta, MockStreamingApiClient,
    };

    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("external-receive.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
          on('session.receive', { origin: 'scheduled-trigger' }, ($, e, next) =>
            e.text === 'drop' ? { consumed: 'muted' }
              : next({ ...e, text: 'scheduled received' }));
          on('session.receive', { origin: 'bridge' }, ($, e, next) =>
            next({ ...e, text: 'bridge received' }));
          on('prompt.submit', ($, e, next) => next({ ...e, text: `${e.text} submitted` }));
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "external-receive",
        dir.path(),
        &module,
        lingxi_core::types::utf16_json::Utf16JsonProjection::parse("{}").unwrap(),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
        crate::scripted![
            message_start("msg_scheduled", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "scheduled answer"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
        crate::scripted![
            message_start("msg_bridge", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "bridge answer"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
    ]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry))),
    );

    orch.run_queued_prompt_batch(
        vec![
            QueuedPromptInput {
                text: source_text("drop"),
                is_meta: true,
                mod_origin: Some(serde_json::json!({"kind":"scheduled-trigger"})),
                ..QueuedPromptInput::default()
            },
            QueuedPromptInput {
                text: source_text("raw scheduled"),
                is_meta: true,
                mod_origin: Some(serde_json::json!({"kind":"scheduled-trigger"})),
                ..QueuedPromptInput::default()
            },
        ],
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    crate::mod_prompt_origin::with_origin(
        serde_json::json!({"kind":"bridge"}),
        orch.run_turn_streaming("raw bridge"),
    )
    .await
    .unwrap();

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 2);
    assert!(calls[0]
        .messages
        .iter()
        .any(|message| message.text_content() == "scheduled received submitted"));
    assert!(!calls[0]
        .messages
        .iter()
        .any(|message| message.text_content().contains("drop")));
    assert!(calls[1]
        .messages
        .iter()
        .any(|message| message.text_content() == "bridge received submitted"));
}
