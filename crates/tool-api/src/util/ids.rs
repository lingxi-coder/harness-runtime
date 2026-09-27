//! Cross-tool invocation-ID generation.
//!
//! Moved here from `tools/src/builtin/file_read.rs` in M8-P5 — it was a
//! `pub(crate)` helper that ~10 tools across categories call, so it belongs
//! in the shared util surface rather than inside the file-read tool.

/// Generate a monotonic, time-prefixed invocation ID (`inv-<micros>-<n>`).
#[must_use]
pub fn ulid_or_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    format!("inv-{micros}-{n}")
}
