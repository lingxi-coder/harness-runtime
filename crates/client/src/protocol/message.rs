//! `MessageDto` + `MessageBlockDto` — the shared block schema reproduced by
//! both `MessageComplete` (F1-03) and a resumed scrollback.
//!
//! Carries the full block set the ~22 tool-card + diff + thinking renderers
//! need so a completed message and a resumed scrollback are reproducible (plan
//! F1-02). Tool payloads are JSON **Strings** (`input_json`/`result_json`) so
//! `serde_json::Value` never enters the contract crate (governing decision
//! §0.4); the diff fields (`old_string`/`new_string`/`file_path`) mirror the
//! TUI `UserToolResult` at `tui/src/state.rs:73-89`.

use serde::{Deserialize, Serialize};

/// A complete conversation message — a role plus an ordered list of content
/// blocks. Reproduces the assistant message a turn produced (or a resumed
/// scrollback entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MessageDto {
    /// The message role (e.g. `"assistant"`, `"user"`).
    pub role: String,
    /// The ordered content blocks comprising the message.
    pub blocks: Vec<MessageBlockDto>,
    /// User-attached images in visual order. The source is a stable URL (a
    /// `data:` URL for inline session bytes, or an existing URL source), so
    /// clients can render resumed media without matching prompt text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "uniffi", uniffi(default = []))]
    pub images: Vec<MessageImageDto>,
    /// Structured scheduled-fire presentation; absent on ordinary messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "uniffi", uniffi(default = None))]
    pub loop_wakeup: Option<LoopWakeupDto>,
}

/// Durable `/loop` fire metadata, rendered without matching user-facing text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LoopWakeupDto {
    /// Host-rendered fire announcement.
    pub message: String,
    /// Optional no-op companion text.
    pub companion: Option<String>,
    /// Cumulative quiet tick count.
    pub streak: u32,
    /// Start of the quiet streak in Unix milliseconds.
    pub since_ms: u64,
}

/// A persisted image projected for transcript renderers.
///
/// `SendPrompt` uses [`crate::protocol::commands::ImageRefDto`] because it needs raw base64 bytes. A
/// resumed transcript uses this URL-shaped projection so desktop and mobile
/// can render the image directly while preserving the same session history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MessageImageDto {
    /// MIME type recovered from the original image source.
    pub media_type: String,
    /// `data:<media_type>;base64,<bytes>` or an existing image URL.
    pub url: String,
}

/// One block within a [`MessageDto`].
///
/// The variant set is the structural parity anchor: it equals the block kinds
/// the TUI scrollback renders (`Text | Thinking | RedactedThinking |
/// CompactBoundary | ToolUse | ToolResult`). `#[non_exhaustive]` so a future
/// block kind is additive (no major bump). Internally tagged on `type`,
/// `snake_case` (the frozen serde convention, decision §0.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageBlockDto {
    /// Plain assistant text.
    Text {
        /// The text body.
        text: String,
    },
    /// Extended-thinking reasoning trace (mirrors `protocol::ContentBlock::Thinking`).
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// Optional cryptographic signature attesting to the trace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// A redacted-thinking block — opaque encrypted reasoning the provider
    /// returns when the trace is withheld.
    RedactedThinking {
        /// The opaque redacted payload.
        data: String,
    },
    /// A compaction boundary reconstructed during session resume. The summary is
    /// hidden by default and may be revealed by clients in their expanded
    /// history view.
    CompactBoundary {
        /// Message count before compaction, when recoverable from metadata.
        messages_before: u32,
        /// Message count after compaction, when recoverable from metadata.
        messages_after: u32,
        /// Full compact summary paired from the transcript-only summary row.
        summary: String,
    },
    /// A tool invocation. `input_json` is the tool input lowered to a JSON
    /// **String** (decision §0.4).
    ToolUse {
        /// Correlator echoed in the matching [`MessageBlockDto::ToolResult`].
        id: String,
        /// Tool name (e.g. `"Read"`, `"Edit"`).
        tool: String,
        /// Tool input as a JSON String.
        input_json: String,
        /// Pre-derived header. Absent on an older engine.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<crate::protocol::tool_display::ToolHeaderDto>,
    },
    /// A tool result. `result_json` is the tool output lowered to a JSON
    /// **String** (decision §0.4). The diff fields mirror the TUI
    /// `UserToolResult` carried at `tui/src/state.rs:73-89` and are `None` for
    /// non-diff tools.
    ToolResult {
        /// Correlator matching the paired [`MessageBlockDto::ToolUse`].
        id: String,
        /// Tool name that returned.
        tool: String,
        /// Tool result as a JSON String.
        result_json: String,
        /// Whether the tool reported failure.
        is_error: bool,
        /// Pre-edit text for diff tools (`old_string` for Edit). `None` for
        /// non-diff tools.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_string: Option<String>,
        /// Post-edit text for diff tools (`new_string` for Edit; `content` for
        /// Write). `None` for non-diff tools.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        new_string: Option<String>,
        /// Edited file path (drives diff syntax language). `None` for non-diff
        /// tools.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_path: Option<String>,
        /// Pre-derived `⎿` block. Absent on an older engine. Supersedes the
        /// three legacy diff fields above, which carry only the raw pair.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<crate::protocol::tool_display::ToolResultDisplayDto>,
    },
}
