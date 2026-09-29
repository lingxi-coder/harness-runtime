//! Pins the exact byte output of `oauth::server_key` for a representative
//! `sse` and `http` spec.
//!
//! `server_key` is the secure-storage lookup key for EVERY persisted MCP
//! OAuth/XAA token (`name|sha256({type,url,headers})[..16]`, see
//! `mcp/src/oauth.rs`). It hashes:
//!   - the `kind()` string literal ("sse" / "http" / ...), and
//!   - the header map in INSERTION order (not sorted).
//!
//! Neither of those inputs has a compiler-enforced link to this hash. If a
//! future edit renames a `McpTransportSpec::kind()` label (e.g. "sse" ->
//! "server-sent-events") or changes `McpHeaders` from an insertion-order map
//! to a sorted one, `cargo check` stays green, every existing behavioural
//! test keeps passing (a fresh login just mints a new token under the new
//! key) — and every already-stored user token silently stops resolving.
//! There is no error path for this: `server_key` just computes a different
//! string, the secure-storage lookup misses, and the server falls back to
//! the "not authenticated" branch. Users are logged out with no diagnostic.
//!
//! This test hard-codes the expected key strings so any such change fails
//! loudly, here, instead of silently in production key-value storage.
#![allow(clippy::unwrap_used)]

use lingxi_core::host::{McpHeaders, McpTransportSpec};
use mcp::oauth;

#[test]
fn server_key_is_frozen_because_it_keys_every_stored_oauth_token() {
    // Representative `sse` spec, one header.
    let mut sse_headers = McpHeaders::new();
    sse_headers.insert("X-Api-Key".to_string(), "secret-token".to_string());
    let sse = McpTransportSpec::Sse {
        url: "https://mcp.example.com/sse".to_string(),
        headers: sse_headers,
        headers_helper: None,
        oauth: None,
    };
    assert_eq!(
        oauth::server_key("my-server", &sse),
        "my-server|36c39242d5b80a46",
        "server_key changed for an `sse` spec: this ORPHANS every user's \
         already-stored OAuth token for every existing `sse` MCP server \
         (secure-storage lookup by the old key silently misses; the config \
         still parses and `cargo check`/every other test stays green)"
    );

    // Representative `http` spec, TWO headers — pins insertion order too
    // (Authorization first, X-Client-Id second; a sorted map would put
    // Authorization first anyway, so this alone would not catch a switch to
    // sorted order — the point is that ANY reordering, sort or otherwise,
    // must show up here rather than only in a live secure-storage miss).
    let mut http_headers = McpHeaders::new();
    http_headers.insert("Authorization".to_string(), "Bearer abc123".to_string());
    http_headers.insert("X-Client-Id".to_string(), "client-9".to_string());
    let http = McpTransportSpec::Http {
        url: "https://api.example.com/mcp".to_string(),
        headers: http_headers,
        headers_helper: None,
        oauth: None,
    };
    assert_eq!(
        oauth::server_key("other-server", &http),
        "other-server|57c827b9f47c3709",
        "server_key changed for an `http` spec (or its header insertion \
         order): this ORPHANS every user's already-stored OAuth token for \
         every existing `http` MCP server with headers"
    );

    // Same two headers, reversed insertion order: must hash to a DIFFERENT
    // key, proving header order really is load-bearing input to this hash
    // (i.e. this test would catch a regression to a sorted/HashMap-backed
    // header type just as it would catch a `kind()` rename).
    let mut reordered = McpHeaders::new();
    reordered.insert("X-Client-Id".to_string(), "client-9".to_string());
    reordered.insert("Authorization".to_string(), "Bearer abc123".to_string());
    let http_reordered = McpTransportSpec::Http {
        url: "https://api.example.com/mcp".to_string(),
        headers: reordered,
        headers_helper: None,
        oauth: None,
    };
    assert_ne!(
        oauth::server_key("other-server", &http_reordered),
        oauth::server_key("other-server", &http),
        "header insertion order stopped affecting server_key — McpHeaders \
         must stay an order-preserving map (see McpHeaders doc comments in \
         platform-api/src/mcp.rs), or stored OAuth tokens keyed under the old \
         order silently orphan"
    );
}
