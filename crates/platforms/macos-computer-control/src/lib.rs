//! Real macOS `ComputerControl` backend (M8-P11b follow-up).
//!
//! Only compiles a working implementation on `target_os = "macos"` — matching
//! claude-code's own darwin-only automation backend
//! (`executor.ts::createCliExecutor` throws off-darwin). Every other target
//! still builds this crate (so the workspace stays green everywhere) but
//! [`new_if_supported`] returns `None`.
//!
//! `unsafe` is unavoidable here (two raw `extern "C"` TCC permission probes in
//! `macos::tcc`) so this crate opts out of the workspace-wide
//! `unsafe_code = "deny"` default, same as `platform-posix`'s one exception.
#![allow(unsafe_code)]

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::MacosComputerControl;

use lingxi_core::host::computer_control::ComputerControl;
use std::sync::Arc;

/// Construct the real backend on macOS, or `None` everywhere else — the one
/// call composition roots need, so `apps/engine-desktop` doesn't need its own
/// `#[cfg(target_os = "macos")]` block.
#[must_use]
pub fn new_if_supported() -> Option<Arc<dyn ComputerControl>> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(MacosComputerControl::new()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}
