//! [`McpTransport`] contract test suite.
//!
//! The full transport-roundtrip tests (real stdio MCP server, real WS bridge)
//! live in `lingxi-mcp` and the platform crates' own `tests/` directories.
//! What we assert here are the trait-level invariants every implementation
//! must honour regardless of which underlying wire format it carries:
//!
//! * `supported_transports()` is non-empty (Unsupported impls still answer
//!   the question rather than panicking).
//! * Connecting with a [`McpTransportKind`] that the impl does NOT claim to
//!   support yields [`McpError::UnsupportedTransport`] — not panic, not hang,
//!   not connect.
//! * `disconnect(unknown_id)` is idempotent (returns Ok or a transport error;
//!   never panics).

use lingxi_core::host::mcp::{McpError, McpTransport, McpTransportKind, McpTransportSpec};
use lingxi_core::types::McpConnectionId;

/// Run the standard [`McpTransport`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn mcp_transport_contract_tests<T: McpTransport>(t: &T) {
    test_supported_transports_non_empty(t);
    test_unclaimed_kind_returns_unsupported(t).await;
    test_disconnect_unknown_id_is_non_fatal(t).await;
}

fn test_supported_transports_non_empty<T: McpTransport>(t: &T) {
    let kinds = t.supported_transports();
    assert!(
        !kinds.is_empty(),
        "supported_transports() must be non-empty (Unsupported impls can still answer)"
    );
}

async fn test_unclaimed_kind_returns_unsupported<T: McpTransport>(t: &T) {
    // claude-code does not expose `InProcess` as a user-configurable
    // transport in settings; production impls mirror that — `connect` must
    // reject it with `McpError::UnsupportedTransport`. Mocks that opt into
    // `InProcess` are exempt and the suite skips this assertion for them.
    let supported = t.supported_transports();
    if supported.contains(&McpTransportKind::InProcess) {
        return;
    }
    let spec = McpTransportSpec::InProcess {
        registry_key: "contract-probe".into(),
    };
    let r = t.connect(&spec).await;
    match r {
        Err(McpError::UnsupportedTransport(_)) => {}
        other => panic!(
            "connect with InProcess spec on a transport that doesn't claim InProcess must return UnsupportedTransport, got {other:?}"
        ),
    }
}

async fn test_disconnect_unknown_id_is_non_fatal<T: McpTransport>(t: &T) {
    let bogus = McpConnectionId::new();
    let r = t.disconnect(bogus).await;
    // Either Ok or a non-panicking error — both are within spec.
    let _ = r;
}
