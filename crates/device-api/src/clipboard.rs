//! `Clipboard` — system-clipboard seam (mobile device capability).
//!
//! Implemented natively in Swift/Kotlin via `UniFFI` and injected into the
//! mobile `Platform`. The `tool-clipboard` tool routes through it so the
//! model can read/write the system clipboard (the engine-driven analog of the
//! camera/voice/share seams) without knowing the native clipboard API. On
//! Android the impl wraps `ClipboardManager`; on iOS, `UIPasteboard`.
//!
//! Modeled as `Option` on the `Platform` (like `stt`/`tts`), so no stub is
//! needed on desktop or non-clipboard platforms.

use async_trait::async_trait;
use thiserror::Error;

/// Failure modes for [`Clipboard`] operations.
#[derive(Debug, Clone, Error)]
pub enum ClipboardError {
    /// The platform does not support this clipboard operation (e.g. Android
    /// 10+ restricts clipboard reads to the focused app / default IME).
    #[error("clipboard operation unsupported")]
    Unsupported,
    /// Any other native failure.
    #[error("clipboard error: {0}")]
    Other(String),
}

/// Native system clipboard (engine-driven read/write of the pasteboard).
#[async_trait]
pub trait Clipboard: Send + Sync {
    /// Write plain text to the system clipboard.
    async fn set_text(&self, text: String) -> Result<(), ClipboardError>;
    /// Read plain text from the system clipboard. Returns `Ok(None)` when the
    /// clipboard is empty or a read is not permitted by the platform (Android
    /// 10+ background-read restriction surfaces as `None`, not an error).
    async fn get_text(&self) -> Result<Option<String>, ClipboardError>;
}
