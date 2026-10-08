//! Conversation execution emits its telemetry events.

use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::{Arc, Mutex as StdMutex};
use telemetry::tengu::orchestrator as orch_events;
use tool_api::registry::ToolRegistry;
use tracing::field::Field;
use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::Registry;

/// Capture every event's `event` field into a Vec<String>.
#[derive(Default, Clone)]
struct EventNameCapture {
    events: Arc<StdMutex<Vec<String>>>,
}

impl<S: Subscriber> Layer<S> for EventNameCapture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        struct V<'a>(&'a mut Option<String>);
        impl tracing::field::Visit for V<'_> {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                if field.name() == "event" {
                    *self.0 = Some(format!("{value:?}").trim_matches('"').to_string());
                }
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                if field.name() == "event" {
                    *self.0 = Some(value.to_string());
                }
            }
        }
        let mut name: Option<String> = None;
        event.record(&mut V(&mut name));
        if let Some(n) = name {
            self.events.lock().unwrap().push(n);
        }
    }
}

#[tokio::test]
async fn run_turn_emits_started_and_completed_in_order() {
    let cap = EventNameCapture::default();
    let subscriber = Registry::default().with(cap.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let resp = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hi".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![resp]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(ToolRegistry::new());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.run_turn("ping").await.expect("happy");

    let events = cap.events.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .any(|n| n == orch_events::CONVERSATION_STARTED),
        "events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|n| n == orch_events::CONVERSATION_COMPLETED),
        "events: {events:?}"
    );
    assert!(
        !events.iter().any(|n| n == orch_events::CONVERSATION_FAILED),
        "no failure expected: {events:?}"
    );
    // Order: STARTED comes before COMPLETED.
    let i_start = events
        .iter()
        .position(|n| n == orch_events::CONVERSATION_STARTED)
        .unwrap();
    let i_end = events
        .iter()
        .position(|n| n == orch_events::CONVERSATION_COMPLETED)
        .unwrap();
    assert!(i_start < i_end, "started < completed: {events:?}");
}

#[tokio::test]
async fn run_turn_emits_failed_on_max_turns_error() {
    let cap = EventNameCapture::default();
    let subscriber = Registry::default().with(cap.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let api = Arc::new(MockApiClient::new(
        (0..5)
            .map(|_| mock_message_response(vec![], Some("max_tokens")))
            .collect(),
    ));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(ToolRegistry::new());

    let cfg = OrchestratorConfig {
        max_turns: 2,
        ..OrchestratorConfig::default()
    };
    let orch = ConversationOrchestrator::new(
        cfg,
        api,
        tools,
        hooks,
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let _err = orch.run_turn("loop").await.expect_err("must fail");

    let events = cap.events.lock().unwrap().clone();
    assert!(
        events
            .iter()
            .any(|n| n == orch_events::CONVERSATION_STARTED),
        "events: {events:?}"
    );
    assert!(
        events.iter().any(|n| n == orch_events::CONVERSATION_FAILED),
        "events: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|n| n == orch_events::CONVERSATION_COMPLETED),
        "no completion expected: {events:?}"
    );
}
