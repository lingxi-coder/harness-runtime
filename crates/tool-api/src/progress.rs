//! Tool progress channel — mpsc channel for streaming progress updates.

use lingxi_core::types::ToolUseId;
use tokio::sync::mpsc;

/// One progress event from a running tool.
#[derive(Debug, Clone)]
pub struct ToolProgress {
    /// Originating tool use ID.
    pub tool_use_id: ToolUseId,
    /// Arbitrary JSON payload (tool-defined).
    pub data: serde_json::Value,
}

/// Sender end of the progress channel.
pub type ToolProgressSender = mpsc::Sender<ToolProgress>;
/// Receiver end of the progress channel.
pub type ToolProgressReceiver = mpsc::Receiver<ToolProgress>;

/// Construct a new bounded (64-slot) progress channel.
#[must_use]
pub fn progress_channel() -> (ToolProgressSender, ToolProgressReceiver) {
    mpsc::channel(64)
}
