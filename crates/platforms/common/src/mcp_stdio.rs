//! Stdio MCP transport helpers — types only in this task; real spawn lives
//! per-platform.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;

/// Configuration passed to `spawn_stdio` on each platform crate.
#[derive(Debug, Clone)]
pub struct StdioConfig {
    /// Executable path or name (resolved via PATH if not absolute).
    pub cmd: String,
    /// Arguments passed verbatim to the child.
    pub args: Vec<String>,
    /// Environment variables (merged with parent env minus filtered secrets).
    pub env: HashMap<String, String>,
    /// Working directory; if `None`, child inherits the parent's cwd.
    pub cwd: Option<PathBuf>,
}

/// Ring buffer with drop-oldest semantics for capturing child stderr.
///
/// claude-code caps MCP child stderr at 64 MB and silently drops the oldest
/// bytes on overflow. This struct implements the same policy.
pub struct StderrRing {
    buf: VecDeque<u8>,
    cap: usize,
    dropped: usize,
}

impl StderrRing {
    /// 64 MB — matches claude-code's `STDERR_BUFFER_CAP`.
    pub const DEFAULT_CAP: usize = 64 * 1024 * 1024;

    /// Construct a ring with the given capacity (in bytes).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
            dropped: 0,
        }
    }

    /// Return the default 64MB cap as a constant function.
    #[must_use]
    pub const fn default_cap() -> usize {
        Self::DEFAULT_CAP
    }

    /// Append bytes; if total would exceed cap, drop oldest first.
    pub fn push(&mut self, bytes: &[u8]) {
        // Fast path: bytes alone exceed cap — keep only the tail.
        if bytes.len() >= self.cap {
            self.dropped += self.buf.len() + (bytes.len() - self.cap);
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let total_after = self.buf.len() + bytes.len();
        if total_after > self.cap {
            let to_drop = total_after - self.cap;
            for _ in 0..to_drop {
                self.buf.pop_front();
            }
            self.dropped += to_drop;
        }
        self.buf.extend(bytes);
    }

    /// Snapshot the current contents as a Vec.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    /// Number of bytes evicted from the front so far.
    #[must_use]
    pub fn dropped_bytes(&self) -> usize {
        self.dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_ring_under_cap_keeps_all_bytes() {
        let mut ring = StderrRing::new(16);
        ring.push(b"hello");
        ring.push(b" world");
        assert_eq!(ring.snapshot(), b"hello world".to_vec());
        assert_eq!(ring.dropped_bytes(), 0);
    }

    #[test]
    fn stderr_ring_over_cap_drops_oldest_bytes() {
        let mut ring = StderrRing::new(5);
        ring.push(b"AAAA"); // 4 bytes, fits
        ring.push(b"BBBB"); // 4 more — total 8 > cap 5, drop 3 oldest
                            // After: oldest 3 of "AAAA" dropped → "ABBBB" (1 'A' kept + 4 'B').
        assert_eq!(ring.snapshot(), b"ABBBB".to_vec());
        assert_eq!(ring.dropped_bytes(), 3);
    }

    #[test]
    fn stderr_ring_single_push_larger_than_cap_truncates_from_head() {
        let mut ring = StderrRing::new(4);
        ring.push(b"123456789"); // 9 bytes into cap 4 → keep last 4
        assert_eq!(ring.snapshot(), b"6789".to_vec());
        assert_eq!(ring.dropped_bytes(), 5);
    }

    #[test]
    fn stderr_ring_uses_64mb_default() {
        assert_eq!(StderrRing::default_cap(), 64 * 1024 * 1024);
    }
}
