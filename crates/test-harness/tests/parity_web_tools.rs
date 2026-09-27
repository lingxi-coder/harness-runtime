//! Parity driver for `web_tools.json` — cross-checks every M4-03 locked
//! literal against the production constants in `lingxi-tools` /
//! `lingxi-telemetry`. Spec §7 + protocol v3 §32.6.

#![allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::used_underscore_binding
)]

use orchestrator::model::betas::WEB_SEARCH as ORCHESTRATOR_WEB_SEARCH_BETA;
use serde::Deserialize;
use telemetry::tengu::tool::{
    WEB_FETCH_COMPLETED, WEB_FETCH_FAILED, WEB_FETCH_STARTED, WEB_SEARCH_COMPLETED,
    WEB_SEARCH_FAILED, WEB_SEARCH_STARTED,
};
use test_harness::parity::load_fixture;
use tool_web::web_fetch::{
    fmt_dns_error, format_http_error_message, WEBFETCH_ALLOWED_SCHEMES, WEBFETCH_MAX_MARKDOWN_LEN,
    WEBFETCH_MAX_REDIRECTS, WEBFETCH_MAX_TRANSFER_BYTES, WEBFETCH_TIMEOUT,
    WEBFETCH_TRUNCATION_SUFFIX, WEBFETCH_USER_AGENT_PREFIX,
};
use tool_web::web_search::{
    WEB_SEARCH_DEFAULT_MAX_TOKENS, WEB_SEARCH_MAX_USES, WEB_SEARCH_TOOL_BLOCK_NAME,
    WEB_SEARCH_TOOL_BLOCK_TYPE,
};

#[derive(Debug, Deserialize)]
struct ErrorTemplates {
    http_status: String,
    dns_failure: String,
}

#[derive(Debug, Deserialize)]
struct WebToolsFixture {
    _source: String,
    _note: String,
    webfetch_max_transfer_bytes: usize,
    webfetch_max_markdown_len: usize,
    webfetch_max_redirects: u32,
    webfetch_truncation_suffix: String,
    webfetch_user_agent_prefix: String,
    webfetch_allowed_schemes: Vec<String>,
    webfetch_timeout_secs: u64,
    websearch_tool_block_type: String,
    websearch_tool_block_name: String,
    websearch_max_uses: u32,
    websearch_default_max_tokens: u32,
    websearch_anthropic_beta: String,
    event_names: Vec<String>,
    error_templates: ErrorTemplates,
}

#[test]
fn webfetch_constants_match_fixture() {
    let fix: WebToolsFixture = load_fixture("web_tools");
    assert!(!fix._source.is_empty());
    assert!(!fix._note.is_empty());
    // TS-faithful caps (utils.ts:112/125/128). The prior single 5 MB body cap
    // (WEBFETCH_MAX_BYTES) was a divergence and is gone.
    assert_eq!(fix.webfetch_max_transfer_bytes, WEBFETCH_MAX_TRANSFER_BYTES);
    assert_eq!(fix.webfetch_max_transfer_bytes, 10 * 1024 * 1024);
    assert_eq!(fix.webfetch_max_markdown_len, WEBFETCH_MAX_MARKDOWN_LEN);
    assert_eq!(fix.webfetch_max_markdown_len, 100_000);
    assert_eq!(fix.webfetch_max_redirects, WEBFETCH_MAX_REDIRECTS);
    assert_eq!(fix.webfetch_max_redirects, 10);
    assert_eq!(fix.webfetch_truncation_suffix, WEBFETCH_TRUNCATION_SUFFIX);
    assert_eq!(fix.webfetch_user_agent_prefix, WEBFETCH_USER_AGENT_PREFIX);
    let allowed: Vec<&str> = fix
        .webfetch_allowed_schemes
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(allowed, WEBFETCH_ALLOWED_SCHEMES);
    assert_eq!(fix.webfetch_timeout_secs, WEBFETCH_TIMEOUT.as_secs());
}

#[test]
fn websearch_constants_match_fixture() {
    let fix: WebToolsFixture = load_fixture("web_tools");
    assert_eq!(fix.websearch_tool_block_type, WEB_SEARCH_TOOL_BLOCK_TYPE);
    assert_eq!(fix.websearch_tool_block_type, "web_search_20250305");
    assert_eq!(fix.websearch_tool_block_name, WEB_SEARCH_TOOL_BLOCK_NAME);
    assert_eq!(fix.websearch_max_uses, WEB_SEARCH_MAX_USES);
    assert_eq!(fix.websearch_max_uses, 8);
    assert_eq!(
        fix.websearch_default_max_tokens,
        WEB_SEARCH_DEFAULT_MAX_TOKENS
    );
    assert_eq!(fix.websearch_anthropic_beta, ORCHESTRATOR_WEB_SEARCH_BETA);
    assert_eq!(fix.websearch_anthropic_beta, "web-search-2025-03-05");
}

#[test]
fn event_names_match_fixture() {
    let fix: WebToolsFixture = load_fixture("web_tools");
    let production = [
        WEB_FETCH_STARTED,
        WEB_FETCH_COMPLETED,
        WEB_FETCH_FAILED,
        WEB_SEARCH_STARTED,
        WEB_SEARCH_COMPLETED,
        WEB_SEARCH_FAILED,
    ];
    let fixture_names: Vec<&str> = fix.event_names.iter().map(String::as_str).collect();
    let production_names: Vec<&str> = production.to_vec();
    assert_eq!(fixture_names, production_names);
    // Defensive: every event MUST end with _started, _completed, or _failed
    // (NOT _succeeded — M3-06 lock).
    for name in &production_names {
        assert!(
            name.ends_with("_started") || name.ends_with("_completed") || name.ends_with("_failed"),
            "event `{name}` violates the M3-06 suffix lock"
        );
    }
}

#[test]
fn error_templates_match_runtime_formatters() {
    let fix: WebToolsFixture = load_fixture("web_tools");

    // PARITY (#93): WebFetch has NO scheme-rejection error — claude-code's
    // `validateURL` never rejects by scheme, so the `scheme_rejected` template
    // was removed from the fixture. `file:`/`data:` URLs are rejected by the
    // single-label-host SSRF rule instead (covered by tool-web unit tests).

    // HTTP >= 400 is now the `iIp` SUCCESS-result body (status sentence + the
    // body note), not the old "WebFetch: HTTP {status} from {url}" transport
    // error. The reason phrase comes from the Node/bun STATUS_CODES table.
    let concrete_http = fix
        .error_templates
        .http_status
        .replace("{status}", "500")
        .replace("{status_text}", "Internal Server Error");
    assert_eq!(concrete_http, format_http_error_message(500, None));

    let concrete_dns = fix
        .error_templates
        .dns_failure
        .replace("{host}", "doesnotexist.invalid");
    assert_eq!(concrete_dns, fmt_dns_error("doesnotexist.invalid"));
}
