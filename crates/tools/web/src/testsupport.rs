//! Test-only shared state guards.

/// Serializes every test that touches the PROCESS-GLOBAL web caches
/// (`cache::URL_CACHE`, `blocklist::DOMAIN_CHECK_CACHE`).
///
/// Both are `static`s shared by the whole test binary, and tests across
/// `web_fetch_test`, `cache` and `blocklist` clear them at entry. Clearing at
/// the top of a test does NOT isolate it: a test running in parallel can clear
/// the entry this one just populated, turning an expected cache HIT into a real
/// fetch that then consumes the wrong mocked response. That is precisely how
/// `second_call_is_served_from_cache` failed — it read the preflight body
/// (`{"can_fetch":true}`) instead of `"cached body"` — and it reproduced only
/// under a saturated full-workspace run, which is how it survived this long.
///
/// Lives here rather than in one test module so the three modules that share
/// the caches also share ONE lock; two separate locks would serialize each
/// module against itself and leave them racing each other.
///
/// EVERY test that touches the fetch path must hold this — not only the ones
/// that CLEAR the caches. The first version of this guard covered only the
/// clearing tests, and the flake came straight back under load: a test that
/// merely populates `URL_CACHE` races one that expects a hit just as surely as
/// a test that wipes it. The invariant is "any user of the shared cache", not
/// "any mutator of it".
///
/// It governs the process-global URL/domain caches AND the process-global
/// `LINGXI_SKIP_WEBFETCH_PREFLIGHT` env var, because those are one resource
/// from a test's point of view: a test that sets the env var makes a
/// concurrent test skip its preflight, which desynchronises that test's queued
/// mock responses and fails it on a body it never asked for. It was originally
/// named `web_cache_lock`, and that name caused the very bug it was meant to
/// prevent — a test guarding the env var reached for a DIFFERENT lock
/// (`SKIP_ENV_LOCK`, which covers `LINGXI_SIMPLE_SYSTEM_PROMPT`) and its
/// comment claimed it therefore could not race a parallel `call()` test. Two
/// locks are not mutual exclusion. Name a lock after the invariant, not after
/// one of the things it happens to protect.
///
/// A `tokio::sync::Mutex` so the guard can be held across the `.await`s these
/// tests contain without tripping `await_holding_lock`.
pub(crate) async fn web_globals_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// [`web_globals_lock`] for a SYNC test. Blocks on the same lock, so a
/// non-`async` test that clears the caches still serializes against the async
/// ones — skipping it there would leave the exact hole the lock exists to
/// close.
pub(crate) fn block_on_web_globals_lock() -> tokio::sync::MutexGuard<'static, ()> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(web_globals_lock())
}

/// RAII guard for `LINGXI_SKIP_WEBFETCH_PREFLIGHT`: sets it on construction and
/// RESTORES the previous value on drop.
///
/// This exists because holding the globals lock while mutating a process-global
/// is only HALF the invariant. `redirect_without_location_returns_http_error_result`
/// took the lock correctly, set the var, and never unset it — so the mutation
/// outlived the critical section, and every preflight-dependent test that
/// happened to be scheduled after it skipped its preflight and consumed the
/// wrong queued mock response. Test order inside a binary is nondeterministic,
/// which is exactly why it presented as a flake rather than a hard failure.
///
/// A lock serializes access; it does not undo what you did while holding it.
/// Restore-on-drop is what makes the section actually atomic.
pub(crate) struct PreflightSkipGuard(Option<String>);

impl PreflightSkipGuard {
    /// Set the var to `value` for as long as the guard lives.
    pub(crate) fn set(value: &str) -> Self {
        let prev = std::env::var(PREFLIGHT_SKIP_ENV).ok();
        std::env::set_var(PREFLIGHT_SKIP_ENV, value);
        Self(prev)
    }

    /// Ensure the var is UNSET for as long as the guard lives.
    pub(crate) fn unset() -> Self {
        let prev = std::env::var(PREFLIGHT_SKIP_ENV).ok();
        std::env::remove_var(PREFLIGHT_SKIP_ENV);
        Self(prev)
    }
}

impl Drop for PreflightSkipGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => std::env::set_var(PREFLIGHT_SKIP_ENV, v),
            None => std::env::remove_var(PREFLIGHT_SKIP_ENV),
        }
    }
}

/// The preflight-skip override. Named once so a typo cannot silently create a
/// second, unguarded variable.
pub(crate) const PREFLIGHT_SKIP_ENV: &str = "LINGXI_SKIP_WEBFETCH_PREFLIGHT";
