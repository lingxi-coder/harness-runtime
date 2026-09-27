//! Unified priority message queue for engine inputs.
//!
//! See spec §27 (`QueuedCommand`, `MessageQueueManager`, `QueueOperation`).

#![forbid(unsafe_code)]

pub mod operations;
pub mod queue;
pub mod telemetry_recorder;

pub use operations::{QueueOperation, QueueOperationRecorder, VecRecorder};
pub use queue::{
    join_prompt_values, MessageQueueManager, NotificationMode, QueuePriority, QueueSource,
    QueuedCommand, QueuedCommandContent,
};
pub use telemetry_recorder::TelemetryQueueRecorder;
