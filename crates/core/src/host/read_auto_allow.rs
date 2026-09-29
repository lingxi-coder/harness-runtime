//! Oracle `kq(e, n, r, s)` — whether the model could actually have READ a path.
//!
//! ```js
//! function mh(e,n){ return n.some(r=>zt(r,e))        // tools contain `e` ("Edit")
//!                       && !n.some(r=>zt(r,rt))      // ...and NOT "Read"
//!                       && !n.some(r=>zt(r,Ui)); }   // ...and NOT "REPL"
//! function kq(e,n,r,s){ return !mh(e,r) && dh(n,s); }
//! ```
//!
//! Two independent conditions, and the naming is easy to get backwards: the
//! first is NOT "Read is available" but "the model is not holding Edit with no
//! way to read" — `mh` is true only when the tool list has the *editing* tool
//! and neither `Read` nor `REPL`. The second, `dh`, asks whether the PATH is
//! readable under the current permission policy.
//!
//! Callers use this to decide whether an edit may proceed against a file whose
//! read-state is stale. ⚠️ Both of them fail OPEN if this answers wrongly —
//! they would let a write land on a file the model was never allowed to see —
//! so the unset case deliberately answers `false`. "Nobody published a probe"
//! must never read as "permitted"; see the `widening a None fails a gate open`
//! family.

use std::sync::Arc;
use std::sync::OnceLock;

/// Session-scoped answer to `kq`. Implemented where both the tool list and the
/// permission policy are in hand; consumed by leaf file tools, which have
/// neither.
pub trait ReadAutoAllow: Send + Sync {
    /// Whether `path` could have been read by this session's model.
    fn read_auto_allowed(&self, path: &str) -> bool;
}

static PROBE: OnceLock<Arc<dyn ReadAutoAllow>> = OnceLock::new();

/// Publish the probe once, at the composition root. Later calls are ignored:
/// the tool list and policy are session-scoped, so a second publisher would be
/// a bug rather than an update.
pub fn set_read_auto_allow_probe(probe: Arc<dyn ReadAutoAllow>) {
    let _ = PROBE.set(probe);
}

/// `kq` for `path`, or `false` when no probe was published.
///
/// ⚠️ The unset answer is `false` ON PURPOSE. Every consumer treats `true` as
/// permission to touch a file whose read-state it cannot vouch for, so an
/// un-wired host must lose that permission, not gain it.
#[must_use]
pub fn read_auto_allowed(path: &str) -> bool {
    PROBE
        .get()
        .is_some_and(|probe| probe.read_auto_allowed(path))
}

/// Whether a probe has been published — for hosts that want to assert their own
/// wiring rather than silently run with the fail-safe answer.
#[must_use]
pub fn read_auto_allow_probe_is_wired() -> bool {
    PROBE.get().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing default. Both consumers read `true` as permission to
    /// write to a file whose read-state they cannot vouch for, so a process
    /// where nobody published a probe must answer `false` — never "permitted by
    /// absence".
    ///
    /// This runs in a process with no probe set, which is exactly the state a
    /// host that forgot to wire one would be in.
    #[test]
    fn an_unwired_process_permits_nothing() {
        assert!(!read_auto_allow_probe_is_wired());
        for path in ["/etc/passwd", "/tmp/a.rs", "", "relative/path.txt"] {
            assert!(
                !read_auto_allowed(path),
                "{path:?} must not be auto-allowed without a published probe"
            );
        }
    }
}
