//! [`SwarmBackend`] contract — trait-level invariants only.
//!
//! Real tmux / iTerm exec tests live in `platforms/posix/src/swarm/` and
//! gate on host-specific env vars (`TMUX_AVAILABLE`, an active iTerm.app on
//! macOS, etc.). This contract sticks to behaviours that hold on every
//! backend — including the always-`Unsupported` Windows impl and the
//! `posix-minimal` stub.
//!
//! Invariants:
//!
//! * `is_available()` answers a `bool` without panicking.
//! * `destroy_swarm` on an unknown handle does not panic or hang — it must
//!   return one of the documented variants (`Ok` for idempotent backends,
//!   `Err(Unsupported)` for refusing platforms, `Err(Tmux)` for backends
//!   that surface the underlying "no such session" error).

use lingxi_core::host::swarm::{SwarmBackend, SwarmError, SwarmHandle};

/// Run the standard [`SwarmBackend`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn swarm_backend_contract_tests<S: SwarmBackend>(s: &S) {
    test_is_available_returns_bool(s);
    test_destroy_swarm_on_unknown_handle(s).await;
}

fn test_is_available_returns_bool<S: SwarmBackend>(s: &S) {
    let _ = s.is_available();
}

async fn test_destroy_swarm_on_unknown_handle<S: SwarmBackend>(s: &S) {
    // Synthesise a handle that the backend has never heard of. The
    // `SwarmHandle` struct is a plain DTO with a `session_name` string, so
    // we use an unmistakably-fake name.
    let handle = SwarmHandle {
        session_name: "lingxi-contract-test-never-real".to_string(),
    };
    let r = s.destroy_swarm(handle).await;
    // Unsupported (Windows + posix-minimal stub) and Tmux("no such session")
    // (real tmux backend) are both fine; what matters is the call returns
    // rather than panicking or hanging.
    match r {
        Ok(()) | Err(SwarmError::Unsupported | SwarmError::Tmux(_)) => {}
    }
}
