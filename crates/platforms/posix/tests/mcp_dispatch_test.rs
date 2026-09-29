//! Verifies that `PosixMcpTransport::connect` dispatches `Sse` and `Http`
//! specs to the shared connectors rather than returning
//! `UnsupportedTransport`.

use lingxi_core::host::{McpError, McpTransport, McpTransportKind, McpTransportSpec};
use platform_posix::PosixMcpTransport;

#[tokio::test]
async fn connect_sse_does_not_return_unsupported_transport() {
    let t = PosixMcpTransport::new();
    let spec = McpTransportSpec::Sse {
        // Use a URL that will fail to connect — we only care that the
        // dispatch arm does NOT short-circuit to UnsupportedTransport.
        url: "http://127.0.0.1:1/never-listens".into(),
        headers: lingxi_core::host::McpHeaders::new(),
        headers_helper: None,
        oauth: None,
    };
    let err = t.connect(&spec).await.expect_err("should fail to connect");
    match err {
        McpError::UnsupportedTransport(k) => {
            panic!("expected Connection error, got UnsupportedTransport({k:?})");
        }
        McpError::Connection(_) => {}
        other => panic!("expected Connection error, got {other:?}"),
    }
}

#[tokio::test]
async fn connect_http_does_not_return_unsupported_transport() {
    // `connect_http` is lazy: it spins up the POST writer task and returns
    // `Ok(Connection)` without doing any I/O. The dispatch test therefore
    // accepts either a successful `connect` OR an error that is NOT
    // `UnsupportedTransport` — both prove the Http arm routes to the shared
    // connector rather than short-circuiting to the unsupported fallthrough.
    let t = PosixMcpTransport::new();
    let spec = McpTransportSpec::Http {
        url: "http://127.0.0.1:1/never-listens".into(),
        headers: lingxi_core::host::McpHeaders::new(),
        headers_helper: None,
        oauth: None,
    };
    if let Err(McpError::UnsupportedTransport(k)) = t.connect(&spec).await {
        panic!("Http arm should not return UnsupportedTransport({k:?})");
    }
}

#[test]
fn supported_transports_includes_sse_and_http() {
    let t = PosixMcpTransport::new();
    let kinds = t.supported_transports();
    assert!(kinds.contains(&McpTransportKind::Sse));
    assert!(kinds.contains(&McpTransportKind::Http));
}
