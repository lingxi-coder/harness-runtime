//! Concrete `AnalyticsSink` implementations: `NoOp` (default), `InMemory` (tests),
//! and the Statsig trait extension point.

pub mod in_memory;
pub mod noop;
pub mod statsig;

pub use in_memory::{InMemorySink, RecordedEvent};
pub use noop::NoOpSink;
pub use statsig::{MockStatsigSink, StatsigSink};

/// Module marker — kept for the Task-1 skeleton-test path resolution.
#[doc(hidden)]
#[must_use]
pub fn _module_marker() -> &'static str {
    "lingxi-telemetry-sinks"
}
