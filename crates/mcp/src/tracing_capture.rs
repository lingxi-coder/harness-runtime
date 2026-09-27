//! Test-only tracing capture, shared by every module that asserts a specific
//! `event`/`reason` pair actually fired (2.1.251 §20b/§23b telemetry-gate
//! wiring) rather than just asserting the pure decision a call site made.
//!
//! Mirrors `orchestrator/tests/orchestrator_telemetry_test.rs`'s
//! `EventNameCapture` layer, extended to also capture a `reason` field (the
//! oracle's `p(gate, reason)` / `y(gate)` shape used throughout this crate's
//! `OTel` log-gate emissions).

use std::sync::{Arc, Mutex as StdMutex};
use tracing::field::Field;
use tracing::Event;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};

/// One captured `(event, reason)` row. A `reason` of `None` means the field
/// was absent (the oracle's `y(gate)` / success shape).
type CapturedRow = (String, Option<String>);

/// Captures every event's `event` and `reason` string fields, in order.
#[derive(Default, Clone)]
pub(crate) struct GateCapture {
    rows: Arc<StdMutex<Vec<CapturedRow>>>,
}

impl GateCapture {
    pub(crate) fn rows(&self) -> Vec<CapturedRow> {
        self.rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl<S: Subscriber> Layer<S> for GateCapture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        struct V {
            event: Option<String>,
            reason: Option<String>,
        }
        impl tracing::field::Visit for V {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                let rendered = format!("{value:?}").trim_matches('"').to_string();
                match field.name() {
                    "event" => self.event = Some(rendered),
                    "reason" => self.reason = Some(rendered),
                    _ => {}
                }
            }
            fn record_str(&mut self, field: &Field, value: &str) {
                match field.name() {
                    "event" => self.event = Some(value.to_string()),
                    "reason" => self.reason = Some(value.to_string()),
                    _ => {}
                }
            }
        }
        let mut v = V {
            event: None,
            reason: None,
        };
        event.record(&mut v);
        if let Some(name) = v.event {
            self.rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((name, v.reason));
        }
    }
}
