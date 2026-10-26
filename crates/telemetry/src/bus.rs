//! `AnalyticsBus` — facade between event emitters and pluggable sinks.
//!
//! See spec §26.3. Engine code calls [`AnalyticsBus::log_event`] on a single
//! process-wide bus. Events submitted before a sink is attached are buffered
//! in a bounded queue and drained when the sink shows up.

use crate::killswitch::Killswitch;
use crate::otel::runtime::mirror_analytics_event;
use crate::sink::{AnalyticsSink, LogEventMetadata};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Single-sink event bus with a bounded pre-attach buffer.
///
/// Construction is cheap (`new` / `default`); the heavy work happens in
/// [`Self::attach_sink`], which drains the buffer in order.
pub struct AnalyticsBus {
    sink: RwLock<Option<Arc<dyn AnalyticsSink>>>,
    pending: Mutex<VecDeque<QueuedEvent>>,
    max_pending: usize,
    killswitch: Killswitch,
}

#[derive(Debug, Clone)]
struct QueuedEvent {
    name: String,
    metadata: LogEventMetadata,
    #[allow(dead_code)]
    is_async: bool,
}

impl AnalyticsBus {
    /// Construct an empty bus with no sink attached and a 1000-event buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sink: RwLock::new(None),
            pending: Mutex::new(VecDeque::new()),
            max_pending: 1000,
            killswitch: Killswitch::new(),
        }
    }

    /// Construct an instance that drops every event before OTEL mirroring,
    /// sink delivery or buffering. Other buses retain their own telemetry state.
    #[must_use]
    pub fn disabled() -> Self {
        let bus = Self::new();
        bus.killswitch.activate();
        bus
    }

    /// Synchronous log. If no sink is attached, the event is buffered (oldest
    /// evicted at capacity). When the killswitch is active the event is
    /// silently dropped.
    ///
    /// The event is also dropped when the traffic-mode privacy gate has
    /// telemetry disabled (CC `F$e()` — `DISABLE_TELEMETRY` / `DO_NOT_TRACK` /
    /// non-essential-traffic). Gating here means every current and future sink
    /// inherits the suppression, matching CC where `F$e()` silences all
    /// `tengu_*` telemetry egress.
    pub async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        if self.killswitch.is_active() {
            self.pending.lock().await.clear();
            return;
        }
        if lingxi_core::host::traffic_mode::is_telemetry_disabled() {
            self.pending.lock().await.clear();
            return;
        }
        mirror_analytics_event(name, &metadata);
        if let Some(sink) = self.sink.read().await.as_ref().cloned() {
            sink.log_event(name, metadata).await;
        } else {
            self.buffer(QueuedEvent {
                name: name.into(),
                metadata,
                is_async: false,
            })
            .await;
        }
    }

    /// Attach an internal analytics sink and drain pending events in FIFO order.
    /// Network sinks must also enforce their export policy; this bus preserves
    /// internal capture independently of the default-off OTel master switch.
    /// Privacy vetoes discard pending events so opting back in cannot replay them.
    pub async fn attach_sink(&self, sink: Arc<dyn AnalyticsSink>) {
        *self.sink.write().await = Some(sink.clone());
        // Drain pending.
        let mut pending = self.pending.lock().await;
        while let Some(e) = pending.pop_front() {
            if self.killswitch.is_active()
                || lingxi_core::host::traffic_mode::is_telemetry_disabled()
            {
                pending.clear();
                break;
            }
            sink.log_event(&e.name, e.metadata).await;
        }
    }

    /// Borrow the killswitch so callers can activate it from outside.
    #[must_use]
    pub fn killswitch(&self) -> &Killswitch {
        &self.killswitch
    }

    async fn buffer(&self, e: QueuedEvent) {
        let mut p = self.pending.lock().await;
        if p.len() >= self.max_pending {
            p.pop_front();
        }
        p.push_back(e);
    }
}

impl Default for AnalyticsBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Policy applied when the pre-attach event buffer overflows.
///
/// v3 §26.3 mandates the bus expose its overflow behaviour to platform
/// init code so operators can choose between losing oldest events,
/// losing newest, or blocking. M3-06 only implements `DropOldest`
/// (matches the existing `buffer()` implementation); the enum is
/// `#[non_exhaustive]` so adding `DropNewest`/`Block` later is
/// non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OverflowPolicy {
    /// When the buffer is full, evict the oldest queued event to make room.
    DropOldest,
}

impl AnalyticsBus {
    /// Return the overflow policy in effect for this bus.
    #[must_use]
    pub fn overflow_policy(&self) -> OverflowPolicy {
        OverflowPolicy::DropOldest
    }

    /// Construct a bus with a [`crate::sinks::NoOpSink`] attached synchronously.
    ///
    /// Use this when the platform has no opinion about telemetry sinks and
    /// wants the "never buffer" guarantee. The bus's existing
    /// [`Self::new`] keeps the "no sink attached, buffer up to 1000 events"
    /// semantics for boot-strap windows where the sink choice depends on
    /// settings loaded later.
    #[must_use]
    pub fn with_default_sink() -> Self {
        use crate::sinks::NoOpSink;
        let bus = Self::new();
        // Synchronous attach: NoOpSink construction is infallible and the bus
        // is freshly built, so no other holder of the lock can exist. Use
        // `try_write` to avoid requiring an async context for this constructor.
        if let Ok(mut guard) = bus.sink.try_write() {
            *guard = Some(Arc::new(NoOpSink));
        }
        bus
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::LogEventMetadata;
    use crate::sinks::InMemorySink;
    use crate::TEST_POLICY_LOCK as ENV_LOCK;

    /// `DISABLE_TELEMETRY` is process-global; serialize the gate cases. No other
    /// test in this crate's unit-test binary drives the bus (the sink tests call
    /// sinks directly), so the window where the var is set is confined here.

    fn clear() {
        for v in [
            "DISABLE_TELEMETRY",
            "DO_NOT_TRACK",
            "LINGXI_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        ] {
            std::env::remove_var(v);
        }
    }

    #[tokio::test]
    async fn disabled_bus_drops_before_buffer_and_sink_without_disabling_main() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        clear();
        let side = AnalyticsBus::disabled();
        let main = AnalyticsBus::new();
        assert!(side.killswitch().is_active());
        assert!(!main.killswitch().is_active());

        side.log_event("side_private_event", LogEventMetadata::default())
            .await;
        assert!(side.pending.lock().await.is_empty());
        let side_sink = Arc::new(InMemorySink::new());
        side.attach_sink(side_sink.clone()).await;
        side.log_event("side_private_event", LogEventMetadata::default())
            .await;
        assert!(side_sink.events().await.is_empty());
        assert!(side.pending.lock().await.is_empty());

        let main_sink = Arc::new(InMemorySink::new());
        main.attach_sink(main_sink.clone()).await;
        main.log_event("main_event", LogEventMetadata::default())
            .await;
        assert_eq!(main_sink.events().await.len(), 1);
    }

    #[tokio::test]
    async fn privacy_veto_discards_pending_events_before_sink_attach() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        clear();
        let bus = AnalyticsBus::new();
        bus.log_event("before_opt_out", LogEventMetadata::default())
            .await;
        assert_eq!(bus.pending.lock().await.len(), 1);
        std::env::set_var("DO_NOT_TRACK", "1");
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;
        clear();
        bus.attach_sink(sink.clone()).await;
        assert!(bus.pending.lock().await.is_empty());
        assert!(sink.events().await.is_empty());
    }

    /// With telemetry enabled (clean env) events reach the sink; once the
    /// privacy gate disables telemetry (CC `F$e()`), `log_event` drops them
    /// before they can reach any sink.
    #[tokio::test]
    async fn privacy_gate_suppresses_events() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();

        let sink = Arc::new(InMemorySink::new());
        let bus = AnalyticsBus::new();
        bus.attach_sink(sink.clone()).await;

        // Clean env ⇒ telemetry enabled ⇒ event delivered.
        bus.log_event("tengu_test_event", LogEventMetadata::default())
            .await;
        assert_eq!(sink.events().await.len(), 1, "delivered when enabled");

        // DO_NOT_TRACK ⇒ F$e()==true ⇒ suppressed.
        std::env::set_var("DO_NOT_TRACK", "1");
        bus.log_event("tengu_test_event", LogEventMetadata::default())
            .await;
        assert_eq!(
            sink.events().await.len(),
            1,
            "no new event under DO_NOT_TRACK"
        );

        clear();
    }
}
