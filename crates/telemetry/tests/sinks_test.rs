//! `NoOpSink` + `InMemorySink` + `with_default_sink` behavior.

use std::collections::HashMap;
use std::sync::Arc;
use telemetry::{AnalyticsBus, AnalyticsSink, AnalyticsValue, InMemorySink, NoOpSink};
use tokio::sync::{Mutex, Notify};

#[tokio::test]
async fn noop_sink_swallows_events_silently() {
    let sink = NoOpSink;
    let mut md = HashMap::new();
    md.insert("k".into(), AnalyticsValue::Int(1));
    sink.log_event("tengu_api_request_started", md).await;
    // No panic = pass; NoOpSink has no observable state.
    assert_eq!(sink.name(), "noop");
}

#[tokio::test]
async fn in_memory_sink_captures_events_in_order() {
    let sink = Arc::new(InMemorySink::new());
    let mut md1 = HashMap::new();
    md1.insert("x".into(), AnalyticsValue::Int(1));
    let mut md2 = HashMap::new();
    md2.insert("y".into(), AnalyticsValue::Int(2));

    sink.log_event("evt_one", md1).await;
    sink.log_event("evt_two", md2).await;

    let events = sink.events().await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].name, "evt_one");
    assert_eq!(events[1].name, "evt_two");
}

#[tokio::test]
async fn in_memory_sink_clear_drops_state() {
    let sink = Arc::new(InMemorySink::new());
    sink.log_event("evt", HashMap::new()).await;
    assert_eq!(sink.events().await.len(), 1);
    sink.clear().await;
    assert_eq!(sink.events().await.len(), 0);
}

#[tokio::test]
async fn analytics_bus_with_default_sink_is_noop() {
    let bus = AnalyticsBus::with_default_sink();
    // No buffer drain needed; sink is attached at construction.
    bus.log_event("evt", HashMap::new()).await;
    // The bus has a NoOp sink; we can't observe through it but we can confirm
    // no panic and the bus is in the attached state by attaching another sink
    // (which would fail noisily if the first attach was buggy).
    let in_mem = Arc::new(InMemorySink::new());
    bus.attach_sink(in_mem.clone()).await;
    bus.log_event("after_swap", HashMap::new()).await;
    let events = in_mem.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "after_swap");
}

struct BlockingSink {
    started: Notify,
    release: Notify,
    events: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl AnalyticsSink for BlockingSink {
    async fn log_event(&self, name: &str, _metadata: HashMap<String, AnalyticsValue>) {
        self.started.notify_one();
        self.release.notified().await;
        self.events.lock().await.push(name.to_string());
    }

    async fn log_event_async(&self, name: &str, metadata: HashMap<String, AnalyticsValue>) {
        self.log_event(name, metadata).await;
    }

    fn name(&self) -> &str {
        "blocking"
    }
}

#[tokio::test]
async fn analytics_bus_log_event_awaits_sink_delivery() {
    let bus = Arc::new(AnalyticsBus::new());
    let sink = Arc::new(BlockingSink {
        started: Notify::new(),
        release: Notify::new(),
        events: Mutex::new(Vec::new()),
    });
    bus.attach_sink(sink.clone()).await;

    let bus_task = {
        let bus = bus.clone();
        tokio::spawn(async move {
            bus.log_event("evt", HashMap::new()).await;
        })
    };

    sink.started.notified().await;
    assert!(
        !bus_task.is_finished(),
        "log_event must not return before the sink finishes its synchronous path"
    );

    sink.release.notify_one();
    bus_task.await.expect("log task must join");
    assert_eq!(sink.events.lock().await.as_slice(), ["evt"]);
}
