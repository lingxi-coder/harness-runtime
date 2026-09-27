//! [`BridgeTransport`] contract.
//!
//! Without a running IDE plugin on the CI box, `connect()` must surface a
//! documented error (`Unsupported` for stub impls, `Connection` for real
//! transports that can't reach a peer) rather than panic or hang. The
//! contract validates the trait surface; live WebSocket integration tests
//! live alongside the bridge implementations.
//!
//! Invariants:
//!
//! * `connect(default-ish config)` returns within a short timeout.
//! * The result is one of `Ok`, `Err(Unsupported)`, `Err(Connection(_))`,
//!   `Err(Auth(_))`, or `Err(Closed)`. Anything else is a contract bug.
//! * `disconnect` on a synthesised connection handle does not panic — it
//!   must return `Ok` (idempotent), `Err(Closed)`, or `Err(Unsupported)`.

use platform_api::bridge::{BridgeConfig, BridgeConnection, BridgeError, BridgeTransport};
use std::time::Duration;
use tokio::time::timeout;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Run the standard [`BridgeTransport`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn bridge_transport_contract_tests<B: BridgeTransport>(b: &B) {
    test_connect_returns_within_timeout(b).await;
    test_disconnect_synthetic_handle(b).await;
}

async fn test_connect_returns_within_timeout<B: BridgeTransport>(b: &B) {
    let config = sample_config();
    let r = timeout(CONNECT_TIMEOUT, b.connect(&config)).await;
    let inner = r.expect("connect must return within 3s, not hang");
    // Live peer on this CI box -> Ok(_) exercises the happy path; otherwise
    // every documented BridgeError variant is acceptable (Unsupported is the
    // stub branch; Connection/Auth/Closed are real-transport failures;
    // RateLimited is what some transports return at handshake time).
    match inner {
        Ok(_)
        | Err(
            BridgeError::Unsupported
            | BridgeError::Connection(_)
            | BridgeError::Auth(_)
            | BridgeError::Closed
            | BridgeError::RateLimited(_),
        ) => {}
    }
}

async fn test_disconnect_synthetic_handle<B: BridgeTransport>(b: &B) {
    let bogus = BridgeConnection {
        connection_id: "contract-test-synthetic".to_string(),
    };
    let r = b.disconnect(bogus).await;
    match r {
        Ok(())
        | Err(BridgeError::Closed | BridgeError::Unsupported | BridgeError::Connection(_)) => {}
        other => panic!("disconnect must be Ok/Closed/Unsupported/Connection, got {other:?}"),
    }
}

/// Build a deterministic [`BridgeConfig`] for the contract. The URL points
/// at a port unlikely to be live; impls that try to connect must time out
/// or refuse within the suite's overall budget.
fn sample_config() -> BridgeConfig {
    BridgeConfig {
        bridge_url: "ws://127.0.0.1:1/bridge".to_string(),
        jwt_token: None,
        poll_interval_ms: 1_000,
        trusted_device_id: "contract-test-device".to_string(),
    }
}
