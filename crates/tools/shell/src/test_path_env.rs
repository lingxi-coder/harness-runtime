//! The crate-wide interlock for `PATH`, shared by every test in this crate.
//!
//! `PATH` is process-global, and this crate's tests use it in two incompatible
//! ways in the SAME test binary:
//!
//! * `powershell` installs a temporary `pwsh` stub — and one test sets `PATH`
//!   to `""` — to exercise discovery. While either is in effect, **no program
//!   in the process can be resolved by bare name**.
//! * `bash_edit_diff` spawns `git` by bare name (in its own fixtures and, via
//!   `ShadowRepo`, in the code under test).
//!
//! Overlapping those two produces `Os { code: 2, kind: NotFound }` out of
//! `Command::output()` — a spawn failure that reads like "git is missing"
//! rather than "a sibling test emptied PATH", and it is intermittent because
//! it depends on the interleaving. Serializing the mutators among themselves is
//! not enough: the readers have to participate too.
//!
//! So the lock is a reader/writer lock over the process's `PATH`, not a mutex:
//! a test that REPLACES `PATH` takes [`write`], a test that RESOLVES a program
//! from `PATH` takes [`read`]. Readers still run concurrently with each other.
//!
//! Both guards are routinely held across `.await`; the tokio test runtimes are
//! per-test, so nothing else on a given runtime can contend for this lock.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

static PATH_ENV: RwLock<()> = RwLock::new(());

/// Take the shared guard: this test resolves a program from `PATH` and must not
/// run while another test has `PATH` replaced.
pub(crate) fn read() -> RwLockReadGuard<'static, ()> {
    PATH_ENV.read().unwrap_or_else(|e| e.into_inner())
}

/// Take the exclusive guard: this test REPLACES `PATH` process-wide.
pub(crate) fn write() -> RwLockWriteGuard<'static, ()> {
    PATH_ENV.write().unwrap_or_else(|e| e.into_inner())
}
