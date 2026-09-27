//! §19 — a static or headers-helper-minted `Authorization` header is
//! authoritative over OAuth for SSE/HTTP MCP servers.
//!
//! Oracle `bo(...)`/`Xt(...)` (services/mcp/auth.ts, byte offset ~@182269537
//! in the 2.1.251 binary) derives two independent booleans BEFORE any OAuth
//! provider is constructed:
//!   - `hasUserAuthHeader` — the server's static config already carries an
//!     `Authorization` header;
//!   - `helperMintsAuthHeader` — the configured `headersHelper`'s dynamic
//!     output actually contains an `Authorization` key (not merely that a
//!     helper is configured — a helper may mint some other header entirely).
//!
//! Either one is authoritative: OAuth is never constructed (so its Bearer
//! can never overwrite the value), checked in that order (`hasUserAuthHeader`
//! first) matching the oracle's `if(h){...} if(_){...}` chain. If the
//! resulting connect attempt still fails with an auth-type response (401 or
//! 403), the failure is reported using the oracle's exact copy —
//! `AUTH_HEADER_REJECTED` / `HEADERS_HELPER_AUTH_REJECTED` — instead of the
//! raw transport error.
//!
//! `FIRST_PARTY_AUTH_REJECTED` (the third code in the same oracle `if` chain,
//! gated on `useFirstPartyAuth`/`firstPartyBearer` with values
//! `design_credential` | `none` | `login` | `design_scoped_login`) is
//! deliberately NOT modelled here: every branch of its message text is about
//! a claude.ai/`/login`/Claude Design first-party bearer — the
//! `claudeai-proxy` connector surface, an explicit non-goal for this port
//! (no first-party-auth concept exists in the Rust registry to trigger it).

use platform_api::{McpError, McpHeaders, McpTransportSpec};

/// Oracle `J8e(e)` (@155987335): `Object.keys(e).some((t)=>t.toLowerCase()===
/// "authorization")`. HTTP field names are case-insensitive (RFC 9110 §5.1),
/// and NOTHING between config/helper parse and this check normalizes them
/// (`expand_header_values` rewrites only values; `parse_helper_output` keeps
/// the helper's JSON keys verbatim), so an exact-key `contains_key` would
/// miss the perfectly legal `authorization` spelling and let OAuth clobber
/// the credential §19 exists to protect. ASCII-only folding is exactly
/// equivalent to JS `toLowerCase()` for this needle (no non-ASCII code point
/// lowercases into any letter of "authorization").
pub(crate) fn has_authorization_key(headers: &McpHeaders) -> bool {
    headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
}

/// `hasUserAuthHeader`: does the server's STATIC config — before any
/// `headersHelper` run or OAuth injection — already carry an `Authorization`
/// header? Only SSE/HTTP/WebSocket specs carry headers at all.
pub(crate) fn spec_has_authorization(spec: &McpTransportSpec) -> bool {
    match spec {
        McpTransportSpec::Sse { headers, .. }
        | McpTransportSpec::Http { headers, .. }
        | McpTransportSpec::WebSocket { headers, .. } => has_authorization_key(headers),
        _ => false,
    }
}

/// The status an auth-type connect failure is reported with, mirroring the
/// oracle's `${statusCode ?? 401}` default.
///
/// The gate is STRUCTURAL, matching oracle `bo`'s
/// `if(!(r instanceof CA||r instanceof DC&&u||d===401||d===403))return;`:
/// `d` is `S instanceof RS ? S.status : S.code`, i.e. either a real numeric
/// HTTP status or a Node error CODE (`"ECONNREFUSED"`), so a plain transport
/// failure can NEVER satisfy it. `registry::error_is_auth_response`'s
/// substring arms (`Connection(m) if m.contains("403")`) are deliberately NOT
/// used here: they fire on any message that merely mentions the digits — a
/// dev server at `http://127.0.0.1:4030/mcp` refusing the connection, or an
/// `MCP_TIMEOUT` containing them — and would replace the real cause with auth
/// copy the oracle would never print. Real 401/403s reach this function
/// structurally (SSE's pre-flight GET and Streamable HTTP's `initialize` both
/// produce `McpError::HttpResponse`, see `registry::error_is_401`).
fn auth_failure_status(e: &McpError) -> Option<u16> {
    match e {
        McpError::HttpResponse {
            status: status @ (401 | 403),
            ..
        } => Some(*status),
        _ => None,
    }
}

/// `AUTH_HEADER_REJECTED` — byte-exact message from the 2.1.251 binary.
fn auth_header_rejected(status: u16) -> McpError {
    McpError::Connection(format!(
        "Server rejected the configured Authorization header (HTTP {status}). Check that the token is valid for this MCP endpoint — OAuth fallback is disabled when headers.Authorization is set."
    ))
}

/// `HEADERS_HELPER_AUTH_REJECTED` — byte-exact message from the 2.1.251
/// binary.
fn headers_helper_auth_rejected(status: u16) -> McpError {
    McpError::Connection(format!(
        "Server rejected the Authorization header minted by the configured headersHelper (HTTP {status}). Check that the helper command returns a valid credential for this MCP endpoint — OAuth fallback is disabled when the helper supplies Authorization."
    ))
}

/// Reclassify a connect failure per the oracle's `hasUserAuthHeader` /
/// `helperMintsAuthHeader` precedence (static checked before helper-minted).
/// A non-auth-type failure, or one where neither flag is set, passes through
/// unchanged — safe to call at every connect-failure exit point.
pub(crate) fn classify_auth_failure(
    error: McpError,
    has_user_auth_header: bool,
    helper_minted_authorization: bool,
) -> McpError {
    let Some(status) = auth_failure_status(&error) else {
        return error;
    };
    if has_user_auth_header {
        return auth_header_rejected(status);
    }
    if helper_minted_authorization {
        return headers_helper_auth_rejected(status);
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_spec(headers: &[(&str, &str)]) -> McpTransportSpec {
        McpTransportSpec::Http {
            url: "https://mcp.example".into(),
            headers: platform_api::McpHeaders::from_iter(
                headers
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
            ),
            headers_helper: None,
            oauth: None,
        }
    }

    #[test]
    fn spec_has_authorization_detects_static_header() {
        assert!(spec_has_authorization(&http_spec(&[(
            "Authorization",
            "Bearer x"
        )])));
        assert!(!spec_has_authorization(&http_spec(&[("X-Static", "yes")])));
    }

    /// Oracle `J8e` lowercases the key before comparing, so every spelling of
    /// the field name counts. A case-SENSITIVE port re-opens the §19 clobber
    /// for the perfectly legal lowercase spelling: `has_user_auth_header`
    /// reads false, OAuth is constructed anyway, and its `Authorization`
    /// entry wins at `HeaderMap::insert` (header names are case-folded on the
    /// wire), discarding the user's static credential.
    #[test]
    fn spec_has_authorization_is_case_insensitive_like_j8e() {
        for spelling in ["authorization", "AUTHORIZATION", "AuThOrIzAtIoN"] {
            assert!(
                spec_has_authorization(&http_spec(&[(spelling, "Bearer static-tok")])),
                "header spelled `{spelling}` must count as hasUserAuthHeader"
            );
        }
        // Not a false positive on a merely similar name.
        assert!(!spec_has_authorization(&http_spec(&[(
            "X-Authorization",
            "Bearer x"
        )])));
        assert!(!spec_has_authorization(&http_spec(&[(
            "Proxy-Authorization",
            "Bearer x"
        )])));
    }

    #[test]
    fn classify_passes_through_non_auth_failures() {
        let e = McpError::Connection("connection refused".into());
        let out = classify_auth_failure(e, true, false);
        assert!(matches!(out, McpError::Connection(m) if m == "connection refused"));
    }

    #[test]
    fn classify_passes_through_when_neither_flag_set() {
        let e = McpError::HttpResponse {
            status: 401,
            www_authenticate: None,
        };
        let out = classify_auth_failure(e, false, false);
        assert!(matches!(out, McpError::HttpResponse { status: 401, .. }));
    }

    #[test]
    fn classify_prefers_static_header_over_helper_minted() {
        // Both flags true (an unusual but possible config): the oracle's
        // `if(h){...} if(_){...}` chain checks `hasUserAuthHeader` first.
        let e = McpError::HttpResponse {
            status: 401,
            www_authenticate: None,
        };
        let out = classify_auth_failure(e, true, true);
        match out {
            McpError::Connection(m) => {
                assert!(m
                    .starts_with("Server rejected the configured Authorization header (HTTP 401)."))
            }
            other => panic!("expected AUTH_HEADER_REJECTED copy, got {other:?}"),
        }
    }

    #[test]
    fn auth_header_rejected_copy_is_byte_exact() {
        let e = McpError::HttpResponse {
            status: 403,
            www_authenticate: None,
        };
        let out = classify_auth_failure(e, true, false);
        assert_eq!(
            out.to_string(),
            "connection failed: Server rejected the configured Authorization header (HTTP 403). Check that the token is valid for this MCP endpoint — OAuth fallback is disabled when headers.Authorization is set."
        );
    }

    #[test]
    fn headers_helper_auth_rejected_copy_is_byte_exact() {
        let e = McpError::HttpResponse {
            status: 401,
            www_authenticate: None,
        };
        let out = classify_auth_failure(e, false, true);
        assert_eq!(
            out.to_string(),
            "connection failed: Server rejected the Authorization header minted by the configured headersHelper (HTTP 401). Check that the helper command returns a valid credential for this MCP endpoint — OAuth fallback is disabled when the helper supplies Authorization."
        );
    }

    /// Oracle `bo` gates on a TYPED/NUMERIC status (`d===401||d===403`, where
    /// `d` is `S instanceof RS ? S.status : S.code` — a Node error code like
    /// `"ECONNREFUSED"` never equals 401). A transport failure whose message
    /// merely CONTAINS those digits (a dev port such as 4030, or an
    /// `MCP_TIMEOUT` of 40100ms) must therefore pass through untouched:
    /// rewriting it as `AUTH_HEADER_REJECTED` destroys the real cause, which
    /// is then what gets stored as the connection's `last_error`.
    #[test]
    fn classify_never_rewrites_a_transport_failure_that_merely_mentions_a_status() {
        for message in [
            "tcp connect error: 127.0.0.1:4030: connection refused",
            "tcp connect error: 127.0.0.1:4010: connection refused",
            "MCP server \"x\" connection timed out after 40300ms",
        ] {
            let out = classify_auth_failure(McpError::Connection(message.into()), true, true);
            assert!(
                matches!(&out, McpError::Connection(m) if m == message),
                "`{message}` must pass through unchanged, got {out:?}"
            );
            let out = classify_auth_failure(McpError::Handshake(message.into()), true, true);
            assert!(
                matches!(&out, McpError::Handshake(m) if m == message),
                "`{message}` must pass through unchanged, got {out:?}"
            );
        }
    }
}
