//! Test-only guard for the process-global env vars this crate's tools read.
//!
//! `glob.rs` and `grep.rs` both read `LINGXI_GLOB_TIMEOUT_SECONDS`, and they
//! live in the SAME test binary, so their tests run on concurrent threads
//! against one process environment. `glob.rs` had a mutex and cleared the var;
//! `grep.rs` mutated it with no lock at all. The result was a load-dependent
//! failure where `grep`'s test set the var to `5`, `glob`'s helper removed it
//! in between, and the next assertion read the 60s WSL default instead.
//!
//! Two rules this crate learned the hard way and encodes here:
//!
//! 1. EVERY user of a shared resource takes the SAME lock — not every mutator,
//!    and not a different lock that happens to be nearby.
//! 2. A process-global mutated under a lock must be RESTORED before the lock is
//!    released. Holding a lock while mutating is only half the invariant: a
//!    mutation that outlives the critical section just serializes the
//!    corruption.

use std::sync::{Mutex, MutexGuard, OnceLock};

/// The env vars `tool-file`'s tools read from the process environment.
const GUARDED: &[&str] = &[
    "LINGXI_GLOB_NO_IGNORE",
    "LINGXI_GLOB_HIDDEN",
    "LINGXI_GLOB_TIMEOUT_SECONDS",
];

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Held for a test body: serializes access to [`GUARDED`] and restores every
/// one of them to its pre-test value on drop.
pub(crate) struct FileEnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl Drop for FileEnvGuard {
    fn drop(&mut self) {
        for (key, prev) in &self.saved {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// Take the lock, snapshot the guarded vars, and CLEAR them so the test starts
/// from a known environment.
pub(crate) fn guard_file_env() -> FileEnvGuard {
    // Recover from a poisoned lock: a panicking test must not wedge every other
    // test in the binary behind it.
    let _lock = lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved = GUARDED
        .iter()
        .map(|k| (*k, std::env::var(k).ok()))
        .collect::<Vec<_>>();
    for k in GUARDED {
        std::env::remove_var(k);
    }
    FileEnvGuard { _lock, saved }
}
