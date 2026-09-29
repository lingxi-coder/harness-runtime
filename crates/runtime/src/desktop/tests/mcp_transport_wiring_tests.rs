use super::new_desktop_mcp_transport;
use platform_api::{McpTransport, McpTransportKind};
use std::sync::Arc;

/// Compile-time coverage for the production composition boundary. The
/// target-specific concrete value must implement both registry traits and
/// must be the platform selected by the current target.
#[test]
fn desktop_mcp_transport_is_target_specific_and_dual_wired() {
    let transport = new_desktop_mcp_transport();
    #[cfg(unix)]
    let _: &platform_posix::PosixMcpTransport = &transport;
    #[cfg(windows)]
    let _: &platform_windows::WindowsMcpTransport = &*transport;

    let kinds = transport.supported_transports();
    assert!(kinds.contains(&McpTransportKind::Sse));
    assert!(kinds.contains(&McpTransportKind::Http));
    #[cfg(windows)]
    {
        assert!(kinds.contains(&McpTransportKind::SseIde));
        assert!(kinds.contains(&McpTransportKind::WsIde));
    }

    let _: Arc<dyn McpTransport> = transport.clone();
    let _: Arc<dyn mcp::RawConnectionProvider> = transport;
}
