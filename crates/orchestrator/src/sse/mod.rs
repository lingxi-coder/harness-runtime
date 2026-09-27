//! Streaming SSE → orchestrator-level event routing.
//!
//! Splits responsibilities cleanly:
//!
//! - [`accumulator::BlockAccumulator`] tracks per-`index` block state for
//!   the duration of one `messages.create` response. Text and `tool_use`
//!   blocks are accumulated; thinking blocks pass through.
//! - [`event_router::dispatch_event`] is the switch that translates each
//!   `StreamEvent` variant into either an accumulator mutation, an
//!   `OutputStream` callback, or a `RouterAction` for the streaming loop
//!   to act on.
//!
//! See M5-04 plan "Reverse-engineered byte-locks" for the source-of-truth
//! claude-code references.
#![forbid(unsafe_code)]

pub mod accumulator;
pub mod event_router;

use thiserror::Error;

/// Failure modes from the per-block accumulator.
///
/// Converted into [`crate::error::OrchestratorError::StreamingProtocol`]
/// at the consumer boundary (`streaming_loop::pump_stream`).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StreamingError {
    /// A delta arrived for an `index` that has no corresponding `start`.
    #[error("streaming: delta for block index {index} without prior start")]
    BlockNotFound {
        /// Block index reported by the API.
        index: u32,
    },
    /// `stop_block(index)` called twice for the same `index`.
    #[error("streaming: double stop for block index {index}")]
    DoubleStop {
        /// Block index reported by the API.
        index: u32,
    },
    /// A delta's tag does not match the started block's tag
    /// (e.g. `input_json_delta` on a `text` block).
    #[error("streaming: type mismatch on block {index}: expected {expected}, got {got}")]
    TypeMismatch {
        /// Block index reported by the API.
        index: u32,
        /// The kind the block was started as.
        expected: &'static str,
        /// The delta variant that arrived.
        got: &'static str,
    },
    /// `input_json_delta` accumulation could not be JSON-parsed at
    /// `content_block_stop`. The buffer is preserved in the error for
    /// debugging.
    #[error("streaming: tool_use input failed to parse as JSON: {reason}: buffer={buffer:?}")]
    ToolUseJsonParse {
        /// Block index reported by the API.
        index: u32,
        /// `serde_json` error description.
        reason: String,
        /// The partial-JSON buffer that failed to parse.
        buffer: String,
    },
}
