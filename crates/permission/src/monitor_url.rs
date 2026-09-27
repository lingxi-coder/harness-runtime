//! WHATWG URL parsing shared by Monitor's schema and permission host checks.

/// Match Monitor's native ASCII WebSocket URL refinement and return its host.
/// Control-character diagnostics are a separate schema refinement.
#[must_use]
pub fn monitor_websocket_url_host(raw: &str) -> Option<String> {
    if !raw.is_ascii() || raw.bytes().any(|b| matches!(b, b'\t' | b'\n' | b'\r')) {
        return None;
    }
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "ws" | "wss")
        || !parsed.username().is_empty()
        || parsed
            .password()
            .is_some_and(|password| !password.is_empty())
    {
        return None;
    }
    parsed.host_str().map(|host| {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned()
    })
}
