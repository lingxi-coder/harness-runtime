//! A [`QueueOperationRecorder`] that forwards queue mutations to the
//! `tracing` observability sink.
//!
//! Parity: claude-code's `MessageQueueManager.logOperation`
//! (`src/utils/messageQueueManager.ts:28-38`) forwards every queue mutation to
//! `recordQueueOperation` (sessionStorage.ts), which appends to the IndexedDB
//! operation log. The Rust twin is the pluggable [`QueueOperationRecorder`]
//! sink (`set_recorder`); telemetry/tracing is the natural Rust home for a
//! production recorder.
//!
//! ## Crate-layering note
//!
//! This impl deliberately depends only on `tracing` (a ubiquitous, low-level
//! crate) and NOT on the `telemetry` crate, so `msgqueue` stays a leaf-level
//! crate with no hard dependency on the telemetry stack. The event-name
//! strings are duplicated here as `&str` literals; the canonical string-lock
//! source lives in `telemetry::tengu::queue` and the
//! `string_lock_matches_telemetry_constants` test (a dev-only dependency)
//! guards the two against drift.

use crate::operations::{QueueOperation, QueueOperationRecorder};
use async_trait::async_trait;

/// `lingxi_queue_enqueued` — locked twin of `telemetry::tengu::queue::ENQUEUED`.
pub(crate) const EVENT_ENQUEUED: &str = "lingxi_queue_enqueued";
/// `lingxi_queue_dequeued` — locked twin of `telemetry::tengu::queue::DEQUEUED`.
pub(crate) const EVENT_DEQUEUED: &str = "lingxi_queue_dequeued";
/// `lingxi_queue_removed` — locked twin of `telemetry::tengu::queue::REMOVED`.
pub(crate) const EVENT_REMOVED: &str = "lingxi_queue_removed";
/// `lingxi_queue_cleared` — locked twin of `telemetry::tengu::queue::CLEARED`.
pub(crate) const EVENT_CLEARED: &str = "lingxi_queue_cleared";

/// Forwards each [`QueueOperation`] to `tracing` as a structured
/// `tracing::info!` event, mirroring the `emit_command_*` helpers in the
/// telemetry crate. Fire-and-forget: a tracing emit never blocks and never
/// fails, so the queue critical path is unaffected even if no subscriber is
/// installed.
#[derive(Debug, Default, Clone, Copy)]
pub struct TelemetryQueueRecorder;

impl TelemetryQueueRecorder {
    /// Construct the recorder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QueueOperationRecorder for TelemetryQueueRecorder {
    async fn record(&self, op: QueueOperation) {
        match op {
            QueueOperation::Enqueue {
                uuid,
                priority,
                source,
            } => {
                tracing::info!(
                    event = EVENT_ENQUEUED,
                    uuid = %uuid,
                    priority = ?priority,
                    source = ?source,
                );
            }
            QueueOperation::Dequeue { uuid } => {
                tracing::info!(event = EVENT_DEQUEUED, uuid = %uuid);
            }
            QueueOperation::Remove { uuid, reason } => {
                tracing::info!(
                    event = EVENT_REMOVED,
                    uuid = %uuid,
                    reason = %reason,
                );
            }
            QueueOperation::Clear { count } => {
                tracing::info!(event = EVENT_CLEARED, count = count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{
        MessageQueueManager, QueuePriority, QueueSource, QueuedCommand, QueuedCommandContent,
    };
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;
    use tracing::field::{Field, Visit};
    use tracing::subscriber::with_default;
    use tracing::{Event, Subscriber};

    /// A captured `tracing` event: its `event` name field plus the names of all
    /// other fields it carried.
    #[derive(Debug, Clone, Default)]
    struct Captured {
        event: String,
        field_names: Vec<String>,
    }

    /// A minimal `tracing::Subscriber` that records each event's `event` field
    /// value and the set of field names. Built on `tracing` core only — no
    /// `tracing-subscriber` dependency — so msgqueue stays dependency-light.
    #[derive(Clone, Default)]
    struct CaptureSubscriber {
        events: Arc<Mutex<Vec<Captured>>>,
    }

    struct CaptureVisitor {
        cap: Captured,
    }

    impl Visit for CaptureVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.cap.field_names.push(field.name().to_string());
            if field.name() == "event" {
                // Debug-formatting a &str yields a quoted string; strip quotes.
                self.cap.event = format!("{value:?}").trim_matches('"').to_string();
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.cap.field_names.push(field.name().to_string());
            if field.name() == "event" {
                self.cap.event = value.to_string();
            }
        }
    }

    impl Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut visitor = CaptureVisitor {
                cap: Captured::default(),
            };
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.cap);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn user_cmd(uuid: &str, priority: QueuePriority) -> QueuedCommand {
        QueuedCommand {
            scheduled_task_id: None,
            scheduled_fire_id: None,
            uuid: uuid.to_string(),
            content: QueuedCommandContent::UserInput {
                text: uuid.to_string(),
            },
            priority,
            queued_at: SystemTime::now(),
            source: QueueSource::PromptInput,
            agent_id: None,
            skip_slash_commands: false,
            is_meta: false,
        }
    }

    /// End-to-end: wire `TelemetryQueueRecorder` onto a real queue, perform every
    /// mutation kind, and assert each fires the matching `lingxi_queue_*` event
    /// with the expected structured fields.
    #[test]
    fn telemetry_recorder_forwards_all_operations() {
        let sub = CaptureSubscriber::default();
        let events = sub.events.clone();

        // `with_default` scopes the capturing subscriber to this async block.
        let fut = async {
            let q = MessageQueueManager::new();
            q.set_recorder(Arc::new(TelemetryQueueRecorder::new()))
                .await;

            q.enqueue(user_cmd("a", QueuePriority::Now)).await;
            q.dequeue().await;
            q.enqueue(user_cmd("b", QueuePriority::Next)).await;
            q.remove(&["b".into()], "test-reason").await;
            q.enqueue(user_cmd("c", QueuePriority::Later)).await;
            q.clear().await;
        };
        // Run the queue mutations under the capturing subscriber. The recorder's
        // `tracing::info!` calls happen on this task, so `with_default` captures.
        with_default(sub.clone(), || {
            futures_lite_block_on(fut);
        });

        let captured = events.lock().unwrap().clone();
        let names: Vec<&str> = captured.iter().map(|c| c.event.as_str()).collect();

        assert!(
            names.contains(&EVENT_ENQUEUED),
            "expected an enqueue event, got {names:?}"
        );
        assert!(
            names.contains(&EVENT_DEQUEUED),
            "expected a dequeue event, got {names:?}"
        );
        assert!(
            names.contains(&EVENT_REMOVED),
            "expected a remove event, got {names:?}"
        );
        assert!(
            names.contains(&EVENT_CLEARED),
            "expected a clear event, got {names:?}"
        );

        // Field-shape assertions per operation kind.
        let enqueue = captured
            .iter()
            .find(|c| c.event == EVENT_ENQUEUED)
            .expect("enqueue captured");
        for f in ["event", "uuid", "priority", "source"] {
            assert!(
                enqueue.field_names.iter().any(|n| n == f),
                "enqueue missing field {f}: {enqueue:?}"
            );
        }

        let remove = captured
            .iter()
            .find(|c| c.event == EVENT_REMOVED)
            .expect("remove captured");
        for f in ["event", "uuid", "reason"] {
            assert!(
                remove.field_names.iter().any(|n| n == f),
                "remove missing field {f}: {remove:?}"
            );
        }

        let clear = captured
            .iter()
            .find(|c| c.event == EVENT_CLEARED)
            .expect("clear captured");
        assert!(
            clear.field_names.iter().any(|n| n == "count"),
            "clear missing count field: {clear:?}"
        );
    }

    /// The locally-duplicated event-name literals MUST match the canonical
    /// string-lock constants in the telemetry crate so the two never drift.
    #[test]
    fn string_lock_matches_telemetry_constants() {
        assert_eq!(EVENT_ENQUEUED, telemetry::tengu::queue::ENQUEUED);
        assert_eq!(EVENT_DEQUEUED, telemetry::tengu::queue::DEQUEUED);
        assert_eq!(EVENT_REMOVED, telemetry::tengu::queue::REMOVED);
        assert_eq!(EVENT_CLEARED, telemetry::tengu::queue::CLEARED);
    }

    /// Tiny single-threaded block-on so the test needs no extra runtime crate
    /// beyond tokio's current-thread executor (used inside `with_default`,
    /// which is sync).
    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
