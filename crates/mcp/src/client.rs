//! `McpClient` wrapping `jsonrpc::Connection`.
//!
//! Full RPC body (initialize, tools/list, tools/call, prompts/list,
//! prompts/get, resources/list, resources/read, ping) lands in M2-02b
//! Tasks 6-12. This file currently exposes only the struct shell + error
//! enum so the public surface in `lib.rs` resolves.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

use futures_util::stream::unfold;
use lingxi_core::host::mcp_result::{
    drive_modern_request, JsonrpcMcpResultIo, McpInputRequiredOptions, McpResultError,
    McpResultProgress,
};
use lingxi_core::host::{
    McpConfiguredToolPolicyDto, McpNegotiatedProtocol, McpNotificationDto, McpNotificationStream,
    McpPermissionCeiling, McpPromptDto, McpProtocolEra, McpResourceContentDto, McpResourceDto,
    McpToolDto, McpToolResultDto, McpTransportKind, ServerCapabilitiesDto,
};
use serde::{Deserialize, Serialize};

use crate::hook_dispatch::HookDispatcher;
use crate::inbound::{new_shared_roots, ElicitationCreateHandler, RootsListHandler, SharedRoots};
use crate::initialize_params::InitializeParams;

fn emit_list_paginated(
    method: &'static str,
    page_count: Option<usize>,
    item_count: usize,
    outcome: &'static str,
    source: Option<&'static str>,
) {
    let payload = telemetry::tengu::mcp::ListPaginatedPayload {
        method: telemetry::Verified::assert_safe(method.to_string()),
        page_count: page_count.map(|count| u32::try_from(count).unwrap_or(u32::MAX)),
        item_count: u32::try_from(item_count).unwrap_or(u32::MAX),
        outcome: telemetry::Verified::assert_safe(outcome.to_string()),
        source: source.map(|value| telemetry::Verified::assert_safe(value.to_string())),
    };
    tracing::info!(
        event = telemetry::tengu::mcp::LIST_PAGINATED,
        method = payload.method.as_str(),
        pageCount = payload.page_count,
        itemCount = payload.item_count,
        outcome = payload.outcome.as_str(),
        source = payload.source.as_ref().map(telemetry::Verified::as_str),
    );
    let mut attrs = BTreeMap::from([
        (
            "method".to_string(),
            telemetry::otel::AttrValue::from(payload.method.as_str().to_string()),
        ),
        (
            "itemCount".to_string(),
            telemetry::otel::AttrValue::from(i64::from(payload.item_count)),
        ),
        (
            "outcome".to_string(),
            telemetry::otel::AttrValue::from(payload.outcome.as_str().to_string()),
        ),
    ]);
    if let Some(page_count) = payload.page_count {
        attrs.insert(
            "pageCount".to_string(),
            telemetry::otel::AttrValue::from(i64::from(page_count)),
        );
    }
    if let Some(source) = payload.source.as_ref() {
        attrs.insert(
            "source".to_string(),
            telemetry::otel::AttrValue::from(source.as_str().to_string()),
        );
    }
    telemetry::otel::emit_named_log_event(telemetry::tengu::mcp::LIST_PAGINATED, &attrs);
}

const MCP_LIST_RETRY_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::from_millis(250),
    std::time::Duration::from_millis(500),
    std::time::Duration::from_millis(1_000),
];

fn retryable_list_connection_error(error: &jsonrpc::ConnectionError) -> bool {
    if let jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(remote)) = error {
        if remote
            .data
            .as_ref()
            .and_then(|data| data.get("httpStatus"))
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|status| (400..500).contains(&status))
        {
            return false;
        }
    }
    if let Some(status) = error
        .to_string()
        .split("MCP_HTTP_STATUS=")
        .nth(1)
        .and_then(|metadata| metadata.split(';').next())
        .and_then(|status| status.parse::<u16>().ok())
    {
        return !(400..500).contains(&status);
    }
    match error {
        jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(remote)) => {
            !matches!(remote.code, -32_001 | -32_602 | -32_601 | -32_600)
                && !(400..500).contains(&remote.code)
        }
        jsonrpc::ConnectionError::Router(
            jsonrpc::RouterError::Timeout(_) | jsonrpc::RouterError::Deserialize(_),
        ) => false,
        jsonrpc::ConnectionError::Router(
            jsonrpc::RouterError::WriterClosed
            | jsonrpc::RouterError::WrongResponseId { .. }
            | jsonrpc::RouterError::Serialize(_),
        )
        | jsonrpc::ConnectionError::Codec(_)
        | jsonrpc::ConnectionError::Broker(_) => true,
    }
}

/// Default UTF-16 length limit for free-form MCP descriptions/instructions.
/// Claude Code 2.1.286 `sB()` permits a positive configured override.
pub const MAX_MCP_DESCRIPTION_LENGTH: usize = 2048;

fn mcp_description_length_limit() -> usize {
    let key = format!("{}MAX_MCP_DESCRIPTION_LENGTH", branding::ENV_PREFIX);
    mcp_description_length_limit_from_env(std::env::var(key).ok().as_deref())
}

fn mcp_description_length_limit_from_env(value: Option<&str>) -> usize {
    let parsed = value.and_then(|value| {
        let value = value.trim_matches(|character| {
            matches!(character,
                '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
                    | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
                    | '\u{205f}' | '\u{3000}' | '\u{feff}'
            )
        });
        let digits = value.strip_prefix('+').unwrap_or(value);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        // The oracle's typed env table uses int({ min: 1, digitsOnly: true }).
        // A finite positive limit larger than usize never truncates any Rust
        // string, so saturate it rather than reverting to the default cap.
        let number = digits.parse::<f64>().ok()?;
        if !number.is_finite() || number < 1.0 {
            return None;
        }
        Some(digits.parse::<usize>().unwrap_or(usize::MAX))
    });
    parsed.unwrap_or(MAX_MCP_DESCRIPTION_LENGTH)
}

/// Truncate using the configured UTF-16 code-unit budget, preserving complete
/// Unicode scalar values. Claude Code 2.1.286 `Yr`/`re` drops a high surrogate
/// at the cut and appends the exact `"… [truncated]"` suffix outside the budget.
/// Returns a borrowed `Cow` when no truncation is required.
#[must_use]
pub fn truncate_description(text: &str) -> Cow<'_, str> {
    truncate_description_at_limit(text, mcp_description_length_limit())
}

fn truncate_description_at_limit(text: &str, limit: usize) -> Cow<'_, str> {
    let mut remaining = limit;
    for (offset, character) in text.char_indices() {
        let units = character.len_utf16();
        if units > remaining {
            return Cow::Owned(format!("{}… [truncated]", &text[..offset]));
        }
        remaining -= units;
    }
    Cow::Borrowed(text)
}

#[cfg(test)]
mod description_truncation_tests {
    use super::*;

    #[test]
    fn description_limit_matches_typed_positive_integer_env() {
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("-1"),
            Some("1.5"),
            Some("2e3"),
            Some("12chars"),
        ] {
            assert_eq!(
                mcp_description_length_limit_from_env(value),
                2048,
                "{value:?}"
            );
        }
        for (value, expected) in [
            ("1", 1),
            ("  +4  ", 4),
            ("\u{feff}+4\u{00a0}", 4),
            ("0002", 2),
            ("8192", 8192),
        ] {
            assert_eq!(mcp_description_length_limit_from_env(Some(value)), expected);
        }
        assert_eq!(
            mcp_description_length_limit_from_env(Some("18446744073709551616")),
            usize::MAX
        );
        assert_eq!(
            mcp_description_length_limit_from_env(Some(&"9".repeat(400))),
            2048
        );
    }

    #[test]
    fn description_truncation_counts_utf16_and_drops_a_split_surrogate_pair() {
        for (text, limit, expected) in [
            ("a😀b", 1, "a… [truncated]"),
            ("a😀b", 2, "a… [truncated]"),
            ("a😀b", 3, "a😀… [truncated]"),
            ("a😀b", 4, "a😀b"),
            ("😀", 1, "… [truncated]"),
            ("e\u{301}好", 2, "e\u{301}… [truncated]"),
            ("", 0, ""),
        ] {
            assert_eq!(truncate_description_at_limit(text, limit), expected);
        }
        let text = "😀".repeat(1025);
        assert_eq!(
            truncate_description_at_limit(&text, 2048),
            format!("{}… [truncated]", "😀".repeat(1024))
        );
    }

    #[test]
    fn description_truncation_preserves_boundary_and_larger_override() {
        let text = "a".repeat(4096);
        assert!(matches!(
            truncate_description_at_limit(&text, 4096),
            Cow::Borrowed(_)
        ));
        assert_eq!(
            truncate_description_at_limit(&text, 2048),
            format!("{}… [truncated]", "a".repeat(2048))
        );
        assert!(matches!(
            truncate_description_at_limit("é😀", 3),
            Cow::Borrowed(_)
        ));
    }
}

/// Strip dangerous Unicode categories from a single string — a 1:1 port of
/// the TypeScript `partiallySanitizeUnicode` from `utils/sanitization.ts`.
///
/// Defends against ASCII Smuggling / Hidden Prompt Injection (HackerOne
/// #3086545): invisible Unicode characters (Tag characters, zero-width
/// joiners, format controls, private-use areas) can be injected by a
/// malicious MCP server into tool names/descriptions where they are
/// invisible to users but visible to the model.
///
/// Steps (mirrors TS exactly):
/// 1. Apply Unicode NFC normalization (Rust `unicode-normalization` NFC;
///    TS uses NFKC — NFKC strips more ligatures/widths but for the
///    dangerous categories removed next, NFC and NFKC are equivalent).
/// 2. Remove `\p{Cf}` (format controls), `\p{Co}` (private-use), `\p{Cn}`
///    (unassigned/noncharacters) — covers the whole dangerous set; matches
///    the TS `[\p{Cf}\p{Co}\p{Cn}]` /gu regex.
/// 3. Apply explicit fallback ranges in case property-class matching misses
///    edge cases (mirrors TS `.replace(...)` chain):
///    - U+200B–U+200F zero-width spaces / LTR-RTL marks
///    - U+202A–U+202E directional formatting
///    - U+2066–U+2069 directional isolates
///    - U+FEFF byte-order mark
///    - U+E000–U+F8FF Basic Multilingual Plane private-use area
/// 4. Repeat until stable (≤10 iterations), then return. TS crashes on
///    10-iteration overflow; here we just return the partially-sanitized
///    string (safe degradation — the caller does not need to crash).
#[must_use]
pub fn partially_sanitize_unicode(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut current = input.nfc().collect::<String>();
    let mut previous = String::new();
    let mut iterations = 0u8;
    const MAX_ITER: u8 = 10;

    while current != previous && iterations < MAX_ITER {
        previous = current.clone();
        // Filter chars belonging to dangerous Unicode general categories.
        // unicode_normalization does not expose category lookups, so we use
        // the `unicode-general-category` crate via the `char_is_dangerous`
        // helper below.
        current = current.chars().filter(|c| !char_is_dangerous(*c)).collect();
        // Explicit fallback ranges (mirrors TS method 2).
        current = current
            .chars()
            .filter(|&c| {
                !matches!(c,
                    '\u{200B}'..='\u{200F}' // zero-width spaces, LTR/RTL marks
                    | '\u{202A}'..='\u{202E}' // directional formatting
                    | '\u{2066}'..='\u{2069}' // directional isolates
                    | '\u{FEFF}'              // byte-order mark
                    | '\u{E000}'..='\u{F8FF}' // BMP private-use area
                )
            })
            .collect();
        iterations += 1;
    }
    current
}

/// Returns `true` when `c` belongs to a dangerous Unicode general category
/// that `partiallySanitizeUnicode` strips: Cf (format controls), Co
/// (private-use), or Cn (unassigned/noncharacters).
///
/// Implemented without an external crate by checking the known code-point
/// ranges for each category, which are stable per Unicode standard.
fn char_is_dangerous(c: char) -> bool {
    let cp = c as u32;
    // Cn (unassigned noncharacters): U+FDD0–U+FDEF and the per-plane
    // noncharacters U+xFFFE / U+xFFFF.
    let is_noncharacter =
        matches!(cp, 0xFDD0..=0xFDEF) || (cp & 0xFFFF) == 0xFFFE || (cp & 0xFFFF) == 0xFFFF;
    // Co (private-use): BMP U+E000–U+F8FF, plane-15 U+F0000–U+FFFFF,
    // plane-16 U+100000–U+10FFFF.
    let is_private_use = matches!(cp, 0xE000..=0xF8FF | 0xF0000..=0xFFFFF | 0x100000..=0x10FFFF);
    // Cf (format controls): a stable enumerated set. Key members:
    // soft-hyphen, directional controls, joiners, tags, variation selectors,
    // interlinear annotation, Arabic/Hebrew specials, etc.
    // We list the contiguous blocks; isolated Cf characters inside blocks
    // already handled by the explicit fallback ranges below are included for
    // completeness (double-removal is harmless).
    let is_format = matches!(
        cp,
        0x00AD        // SOFT HYPHEN
        | 0x0600..=0x0605 // Arabic number signs
        | 0x061C        // Arabic Letter Mark
        | 0x06DD        // Arabic End of Ayah
        | 0x070F        // Syriac Abbreviation Mark
        | 0x0890..=0x0891 // Arabic pound/piastre
        | 0x08E2        // Arabic disputed end of ayah
        | 0x180E        // Mongolian vowel separator
        | 0x200B..=0x200F // zero-width + LTR/RTL
        | 0x202A..=0x202E // directional formatting
        | 0x2060..=0x2064 // word joiner + invisible
        | 0x2066..=0x206F // directionality + deprecated
        | 0xFEFF        // BOM / ZWNBSP
        | 0xFFF9..=0xFFFB // interlinear annotation
        | 0x110BD       // Kaithi number sign
        | 0x110CD       // Kaithi number sign above
        | 0x13430..=0x1343F // Egyptian Hieroglyph formatting
        | 0x1BCA0..=0x1BCA3 // Shorthand format controls
        | 0x1D173..=0x1D17A // Musical symbol begin/end
        | 0xE0001        // Language tag
        | 0xE0020..=0xE007F // Tags block
    );
    is_noncharacter || is_private_use || is_format
}

/// Recursively sanitize all string values in a `serde_json::Value`, 1:1 with
/// the TypeScript `recursivelySanitizeUnicode` from `utils/sanitization.ts`.
///
/// * Strings → [`partially_sanitize_unicode`]
/// * Arrays → element-wise recursion
/// * Objects → key + value recursion
/// * Primitives (number, bool, null) → unchanged
#[must_use]
pub fn recursively_sanitize_unicode(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(partially_sanitize_unicode(&s)),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(recursively_sanitize_unicode).collect())
        }
        serde_json::Value::Object(obj) => {
            let sanitized = obj
                .into_iter()
                .map(|(k, v)| {
                    let k2 = partially_sanitize_unicode(&k);
                    (k2, recursively_sanitize_unicode(v))
                })
                .collect();
            serde_json::Value::Object(sanitized)
        }
        other => other, // numbers, booleans, null — unchanged
    }
}

/// Parsed body of an MCP `initialize` response.
///
/// `capabilities` is left as a raw JSON value because the wire shape uses
/// per-feature *objects* (e.g. `"tools": {}`) but the trait DTO only
/// surfaces booleans. The conversion happens in [`decode_server_capabilities`].
#[derive(Debug, Deserialize)]
struct InitializeResponse {
    #[serde(default)]
    capabilities: serde_json::Value,
    /// Server-provided free-form instructions appended to the system prompt.
    /// Truncated to [`MAX_MCP_DESCRIPTION_LENGTH`] on receipt (matches
    /// `claude-code/src/services/mcp/client.ts:1163-1166`).
    #[serde(default)]
    instructions: Option<String>,
}

/// Decode the server's capability JSON object into the trait DTO.
///
/// MCP servers send capability *objects* (e.g. `"tools": {}`); the DTO
/// collapses the common wire shape down to booleans. Presence of the key
/// (even with an empty object value) is treated as `true`. The one strict
/// extension bit currently lifted into the DTO is
/// `io.modelcontextprotocol/skills.directoryRead`.
fn decode_server_capabilities(raw: &serde_json::Value) -> ServerCapabilitiesDto {
    use std::collections::HashMap;
    let obj = raw.as_object();
    let has = |k: &str| obj.is_some_and(|o| o.contains_key(k));
    let directory_read = obj
        .and_then(|o| o.get("extensions"))
        .and_then(|v| v.get("io.modelcontextprotocol/skills"))
        .and_then(serde_json::Value::as_object)
        .and_then(|ext| ext.get("directoryRead"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let experimental: HashMap<String, serde_json::Value> = obj
        .and_then(|o| o.get("experimental"))
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    ServerCapabilitiesDto {
        tools: has("tools"),
        resources: has("resources"),
        prompts: has("prompts"),
        logging: has("logging"),
        directory_read,
        experimental,
        extensions: obj
            .and_then(|o| o.get("extensions"))
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
    }
}

pub(crate) fn modern_meta(
    version: &str,
    elicitation: lingxi_core::host::McpElicitationMode,
) -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientInfo": {
            "name": crate::CLIENT_NAME,
            "title": crate::CLIENT_TITLE,
            "version": env!("CARGO_PKG_VERSION"),
            "description": "An agentic coding tool",
            "websiteUrl": crate::MCP_WEBSITE_URL,
        },
        "io.modelcontextprotocol/clientCapabilities": lingxi_core::host::mcp_client_capabilities(elicitation),
    })
}

fn modern_request_requires_meta(method: &str) -> bool {
    matches!(
        method,
        "tools/list"
            | "tools/call"
            | "subscriptions/listen"
            | "completion/complete"
            | "prompts/list"
            | "prompts/get"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
            | "resources/directory/read"
    )
}

fn modern_request_requires_result_type(method: &str) -> bool {
    modern_request_requires_meta(method)
}

/// Async MCP client built on top of a [`jsonrpc::Connection`].
///
/// One instance per server connection; owns its `Connection` and inbound
/// handler registrations.
pub struct McpClient {
    /// Same frozen capability authority used by the owning connection attempt.
    elicitation: lingxi_core::host::McpElicitationCapabilities,
    /// claude-code `transportErrorState.pendingElicitations` — how many
    /// elicitations this connection has OPEN right now. Shared with the
    /// registered `elicitation/create` handler, which owns the increment and
    /// the decrement-on-drop; see [`Self::has_pending_elicitation`].
    elicitation_pending: Arc<AtomicUsize>,
    /// Logical server name (used in tool full-names and error messages).
    server_name: String,
    /// Absolute cwd advertised to the server via `roots/list`.
    #[allow(dead_code)] // read indirectly via the registered RootsListHandler
    cwd: PathBuf,
    /// Underlying JSON-RPC connection produced by the platform transport.
    #[allow(dead_code)] // wired further in Tasks 9-12 (tools/list, ...)
    connection: Arc<jsonrpc::Connection>,
    /// Server capabilities snapshot from the `initialize` response.
    server_capabilities: RwLock<Option<ServerCapabilitiesDto>>,
    /// The RAW `capabilities` object from the `initialize` response, kept
    /// alongside the decoded [`ServerCapabilitiesDto`].
    ///
    /// [`decode_server_capabilities`] now carries the strict
    /// `directory_read` bit into [`ServerCapabilitiesDto`], so the production
    /// directory-listing path no longer depends on this raw cache. The raw
    /// object is still kept for direct-client initialize semantics, precise
    /// extension reads beyond the DTO surface, and byte-faithful tests.
    raw_server_capabilities: RwLock<Option<serde_json::Value>>,
    /// Server-provided instructions string from the `initialize` response,
    /// truncated to [`MAX_MCP_DESCRIPTION_LENGTH`] chars on receipt
    /// (matches claude-code `client.ts:1163-1166`).
    server_instructions: RwLock<Option<String>>,
    /// Per-server `tools/call` timeout (ms) from the resolved
    /// [`crate::McpServerConfig::timeout_ms`]; fed to [`mcp_tool_timeout_for`]
    /// (`BHs`). `None` = fall back to the `MCP_TOOL_TIMEOUT` env / default.
    config_timeout_ms: Option<u64>,
    /// Server-level `alwaysLoad` ([`crate::McpServerConfig::always_load`]): when
    /// `true`, every tool this client lists is marked `always_load` so it is
    /// never deferred behind tool search.
    config_always_load: bool,
    /// Config-side per-tool declarations used to resolve permission policy.
    config_tools: Vec<McpConfiguredToolPolicyDto>,
    /// Effective host/org ceilings keyed by raw upstream tool name.
    config_tool_permissions: BTreeMap<String, McpPermissionCeiling>,
    /// Strictest ceiling computed for the most recent tools/list response.
    /// Kept out of `McpToolDto` to preserve the discovery-cache wire shape.
    effective_permission_ceilings: std::sync::RwLock<BTreeMap<String, McpPermissionCeiling>>,
    /// Transport kind of the underlying connection, feeding the `GLd` idle-timeout
    /// resolver ([`mcp_tool_idle_timeout_for`]): stdio → 30 min default, remote →
    /// 5 min, in-process (IDE/SDK) → no idle timeout. Defaults to
    /// [`McpTransportKind::Stdio`] (claude-code's `e?.type ?? "stdio"`) until the
    /// registry sets the real kind via [`Self::with_transport_kind`].
    transport_kind: McpTransportKind,
    /// The server's connection URL, when it has one (`None` for `stdio`).
    /// Feeds the §20a per-server schema-normalization gate
    /// ([`crate::tool_schema::decide_tool_schema`]) the same way
    /// `protocol_negotiation.rs`'s denylist gate consults a server's URL.
    /// Defaults to `None` (byte-identical to omitting [`Self::with_server_url`]
    /// entirely) until a caller threads the resolved
    /// [`crate::McpTransportSpec`]'s URL through — no production call site
    /// does yet; see the §20a batch report.
    server_url: Option<String>,
    /// Protocol result envelope selected by the transport handshake.
    negotiated: McpNegotiatedProtocol,
}

impl McpClient {
    /// Subscribe to notifications from this live server connection.
    ///
    /// The subscription owns a fresh JSON-RPC broadcast receiver, so each
    /// caller receives an independent stream and dropping it cannot affect
    /// the client or any other subscriber. A lagged receiver emits a synthetic
    /// list-changed notification so callers immediately reconcile against the
    /// authoritative catalog instead of waiting for another server push.
    #[must_use]
    pub fn subscribe_notifications(&self) -> McpNotificationStream {
        let receiver = self.connection.notifications();
        let stream = unfold(receiver, |mut receiver| async move {
            match receiver.recv().await {
                Ok(notification) => Some((
                    McpNotificationDto {
                        method: notification.method,
                        params: notification.params.unwrap_or(serde_json::Value::Null),
                    },
                    receiver,
                )),
                // A slow subscriber may have missed the only catalog
                // invalidation in a burst. Force an authoritative resource
                // reconciliation instead of waiting indefinitely for a
                // later notification that may never arrive.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => Some((
                    McpNotificationDto {
                        method: "notifications/resources/list_changed".to_string(),
                        params: serde_json::json!({ "lagged": skipped }),
                    },
                    receiver,
                )),
                // Closing the connection naturally ends this subscription.
                Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
            }
        });
        Box::pin(stream)
    }

    /// Build a new client wrapping a JSON-RPC `Connection`.
    ///
    /// Registers two inbound request handlers required by the
    /// roots and form/URL elicitation capability advertisement:
    ///
    /// * `roots/list` -> [`RootsListHandler`] returning `file://<cwd>`.
    /// * `elicitation/create` -> [`ElicitationCreateHandler`] returning
    ///   `{"action":"cancel"}` (default-deny until the host UI replaces it).
    ///
    /// The platform-side caller is responsible for resolving `cwd` to an
    /// absolute path before passing it here (typically via
    /// `std::env::current_dir()`).
    ///
    /// Async because `jsonrpc::Connection::register_handler` is async
    /// (the dispatcher map is behind an async `RwLock`). The plan's pseudo-
    /// signature was synchronous; the real M2-02a API requires `.await`.
    pub async fn new(
        server_name: impl Into<String>,
        cwd: PathBuf,
        connection: Arc<jsonrpc::Connection>,
    ) -> Self {
        Self::with_hook_dispatcher(server_name, cwd, connection, None).await
    }

    /// Like [`Self::new`], but wires an optional [`HookDispatcher`] into the
    /// registered [`ElicitationCreateHandler`] so an incoming
    /// `elicitation/create` can consult the `Elicitation` hook.
    ///
    /// `dispatcher == None` is byte-identical to [`Self::new`]: the handler
    /// keeps its default `{"action":"cancel"}` behavior. `Some(_)` enables the
    /// hook fire-and-resolve path (claude-code `runElicitationHooks`).
    ///
    /// Advertises ONLY `cwd` on `roots/list` (no additional working dirs) —
    /// use [`Self::with_roots`] to also advertise `--add-dir` / settings
    /// `additionalDirectories` roots.
    pub async fn with_hook_dispatcher(
        server_name: impl Into<String>,
        cwd: PathBuf,
        connection: Arc<jsonrpc::Connection>,
        dispatcher: Option<Arc<dyn HookDispatcher>>,
    ) -> Self {
        Self::with_roots(
            server_name,
            cwd,
            new_shared_roots(Vec::new()),
            connection,
            dispatcher,
        )
        .await
    }

    /// Like [`Self::with_hook_dispatcher`], but also advertises the session's
    /// LIVE additional working directories alongside `cwd` on `roots/list`
    /// (settings `additionalDirectories` union CLI `--add-dir`, plus any
    /// runtime `/add-dir`).
    ///
    /// `additional_roots` is a shared [`SharedRoots`] cell, NOT a snapshot: the
    /// registered [`RootsListHandler`] reads it fresh on every `roots/list`, so
    /// a directory pushed into the cell at runtime (paired with
    /// [`Self::send_roots_list_changed`]) is reflected without a reconnect.
    ///
    /// Matches claude-code 2.1.207 `r1d()`, which returns `roots/list` as
    /// `[cwd, ...additionalWorkingDirectories]` deduped by file URL. Passing an
    /// empty cell is byte-identical to [`Self::with_hook_dispatcher`].
    pub async fn with_roots(
        server_name: impl Into<String>,
        cwd: PathBuf,
        additional_roots: SharedRoots,
        connection: Arc<jsonrpc::Connection>,
        dispatcher: Option<Arc<dyn HookDispatcher>>,
    ) -> Self {
        let server_name = server_name.into();
        connection
            .register_handler(
                "roots/list",
                Arc::new(RootsListHandler {
                    cwd: cwd.clone(),
                    additional: additional_roots,
                }),
            )
            .await;
        // Shared with the handler so the auto-background race can ask this
        // client whether a dialog is open (MON-08). One Arc, two owners.
        let elicitation_pending = Arc::new(AtomicUsize::new(0));
        connection
            .register_handler(
                "elicitation/create",
                Arc::new(ElicitationCreateHandler::with_dispatcher_and_counter(
                    server_name.clone(),
                    dispatcher,
                    Arc::clone(&elicitation_pending),
                )),
            )
            .await;

        Self {
            elicitation_pending,
            elicitation: lingxi_core::host::McpElicitationCapabilities::default(),
            server_name,
            cwd,
            connection,
            server_capabilities: RwLock::new(None),
            raw_server_capabilities: RwLock::new(None),
            server_instructions: RwLock::new(None),
            config_timeout_ms: None,
            config_always_load: false,
            config_tools: Vec::new(),
            config_tool_permissions: BTreeMap::new(),
            effective_permission_ceilings: std::sync::RwLock::new(BTreeMap::new()),
            transport_kind: McpTransportKind::Stdio,
            server_url: None,
            negotiated: McpNegotiatedProtocol {
                era: McpProtocolEra::Legacy,
                version: "2025-11-25".to_string(),
            },
        }
    }

    /// Builder that attaches the resolved per-server config options — the
    /// [`crate::McpServerConfig::timeout_ms`] (folded `timeout`/
    /// `request_timeout_ms`) and [`crate::McpServerConfig::always_load`]. The
    /// timeout feeds the `BHs` per-call resolver ([`mcp_tool_timeout_for`]) and
    /// the `always_load` flag is OR'd into every tool's `always_load` bit at
    /// [`Self::list_tools`] time. Passing empty policy collections is byte-identical to
    /// not calling this at all.
    #[must_use]
    pub fn with_config_options(
        mut self,
        timeout_ms: Option<u64>,
        always_load: bool,
        tools: Vec<McpConfiguredToolPolicyDto>,
        tool_permissions: BTreeMap<String, McpPermissionCeiling>,
    ) -> Self {
        self.config_timeout_ms = timeout_ms;
        self.config_always_load = always_load;
        self.config_tools = tools;
        self.config_tool_permissions = tool_permissions;
        self
    }

    /// Builder that records the connection's transport kind so the `GLd`
    /// idle-timeout resolver ([`mcp_tool_idle_timeout_for`]) picks the right
    /// silence-window default (stdio 30 min / remote 5 min / in-process none) and
    /// caps it by the per-call [`mcp_tool_timeout_for`] ceiling. Defaults to
    /// [`McpTransportKind::Stdio`] when not called (byte-identical to claude-code's
    /// `e?.type ?? "stdio"`).
    #[must_use]
    pub fn with_transport_kind(mut self, kind: McpTransportKind) -> Self {
        self.transport_kind = kind;
        if lingxi_core::host::McpElicitationCapabilities::for_transport(kind).legacy
            == lingxi_core::host::McpElicitationMode::Bare
        {
            self.elicitation.legacy = lingxi_core::host::McpElicitationMode::Bare;
        }
        self
    }

    /// Builder that records the server's connection URL (`None` for
    /// `stdio`/url-less transports) so [`Self::list_tools`] can consult the
    /// §20a per-server schema-normalization gate
    /// ([`crate::tool_schema::decide_tool_schema`]) the same way a remote
    /// server's URL feeds `protocol_negotiation.rs`'s denylist gate. Not
    /// calling this is byte-identical to a url-less server for that gate
    /// (only a bare `"*"` allowlist entry can still match).
    #[must_use]
    pub fn with_server_url(mut self, url: Option<String>) -> Self {
        self.server_url = url;
        self
    }

    /// Record the exact protocol result selected by the transport. The
    /// registry passes this from the same handshake result it stores in the
    /// live connection state, so direct client calls cannot drift eras.
    #[must_use]
    pub fn with_negotiated_protocol(mut self, negotiated: McpNegotiatedProtocol) -> Self {
        self.negotiated = negotiated;
        self
    }

    /// Install the host's current capability authority without recomputing feature gates.
    #[must_use]
    pub fn with_elicitation_capabilities(
        mut self,
        elicitation: lingxi_core::host::McpElicitationCapabilities,
    ) -> Self {
        self.elicitation = elicitation;
        self
    }

    pub(crate) fn elicitation_capabilities(&self) -> lingxi_core::host::McpElicitationCapabilities {
        self.elicitation
    }

    /// Adopt the handshake already performed by the host transport. Repeating
    /// initialize here would invalidate modern negotiated connections.
    #[must_use]
    pub fn with_handshake_state(
        mut self,
        capabilities: ServerCapabilitiesDto,
        metadata: Option<&lingxi_core::host::McpServerMetadataDto>,
    ) -> Self {
        *self.raw_server_capabilities.get_mut() = metadata
            .and_then(|metadata| metadata.raw_capabilities.as_ref())
            .cloned();
        *self.server_capabilities.get_mut() = Some(capabilities);
        *self.server_instructions.get_mut() = metadata
            .and_then(|metadata| metadata.instructions.as_deref())
            .map(|instructions| truncate_description(instructions).into_owned());
        self
    }

    fn modern(&self) -> bool {
        self.negotiated.era == McpProtocolEra::Modern
    }

    fn request_params(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        if !self.modern() || !modern_request_requires_meta(method) {
            return params;
        }
        let mut object = params.as_object().cloned().unwrap_or_default();
        let mut required = modern_meta(&self.negotiated.version, self.elicitation.modern)
            .as_object()
            .cloned()
            .unwrap_or_default();
        if let Some(existing) = object
            .remove("_meta")
            .and_then(|value| value.as_object().cloned())
        {
            for (key, value) in existing {
                required.entry(key).or_insert(value);
            }
        }
        object.insert("_meta".to_string(), serde_json::Value::Object(required));
        serde_json::Value::Object(object)
    }

    async fn request_result(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpResultError> {
        let params = self.request_params(method, params);
        if self.modern() && modern_request_requires_result_type(method) {
            drive_modern_request(
                &JsonrpcMcpResultIo {
                    connection: self.connection.clone(),
                    client_capabilities: modern_meta(
                        &self.negotiated.version,
                        self.elicitation.modern,
                    )["io.modelcontextprotocol/clientCapabilities"]
                        .clone(),
                },
                method,
                params,
                McpInputRequiredOptions::default(),
            )
            .await
        } else {
            self.connection
                .call(method, params)
                .await
                .map_err(McpResultError::Connection)
        }
    }

    async fn call_modern_tool(
        &self,
        tool: &str,
        input: serde_json::Value,
        timeout: std::time::Duration,
        tool_use_id: Option<&str>,
        on_progress: Option<McpProgressCallback>,
    ) -> Result<McpToolResultDto, McpClientError> {
        let mut params = serde_json::json!({"name":tool,"arguments":input});
        if let Some(id) = tool_use_id {
            params["_meta"] = serde_json::json!({"claudecode/toolUseId":id});
        }
        let params = self.request_params("tools/call", params);
        let last_activity = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));
        let callback = on_progress
            .filter(|_| tool_use_id.is_some())
            .map(|callback| {
                let activity = last_activity.clone();
                Arc::new(move |event: McpResultProgress| {
                    if let Ok(mut instant) = activity.lock() {
                        *instant = tokio::time::Instant::now();
                    }
                    callback(McpProgressEvent {
                        progress: event.progress,
                        total: event.total,
                        message: event.message,
                    });
                }) as lingxi_core::host::mcp_result::McpResultProgressCallback
            });
        let options = McpInputRequiredOptions {
            per_request_timeout: timeout,
            max_total_timeout: Some(timeout),
            on_progress: callback,
            ..Default::default()
        };
        let io = JsonrpcMcpResultIo {
            connection: self.connection.clone(),
            client_capabilities: modern_meta(&self.negotiated.version, self.elicitation.modern)
                ["io.modelcontextprotocol/clientCapabilities"]
                .clone(),
        };
        let request = drive_modern_request(&io, "tools/call", params, options);
        let idle_timeout = mcp_tool_idle_timeout_for(self.config_timeout_ms, self.transport_kind);
        let value = if idle_timeout.is_zero() {
            request
                .await
                .map_err(|e| self.tool_result_error(e, tool, timeout))?
        } else {
            tokio::select! {
                value=request=>value.map_err(|e|self.tool_result_error(e,tool,timeout))?,
                idle=idle_watchdog(idle_timeout,last_activity,self.server_name.clone(),tool.to_owned())=>return idle,
            }
        };
        serde_json::from_value::<ToolCallResponse>(value)
            .map(|resp| McpToolResultDto {
                content: resp.content,
                is_error: resp.is_error,
                meta: resp.meta,
                structured_content: resp.structured_content,
            })
            .map_err(|e| McpClientError::Deserialize(e.to_string()))
    }

    fn tool_result_error(
        &self,
        error: McpResultError,
        tool: &str,
        timeout: std::time::Duration,
    ) -> McpClientError {
        if matches!(&error,McpResultError::Sdk(error) if error.code == "REQUEST_TIMEOUT") {
            McpClientError::Timeout {
                server: self.server_name.clone(),
                tool: tool.to_owned(),
                secs: timeout.as_secs().max(1),
            }
        } else {
            mcp_client_error_from_result(error)
        }
    }

    /// Send `notifications/roots/list_changed` to the server, telling it the
    /// client's working-dir set changed so it should re-query `roots/list`.
    ///
    /// Backs the `roots.listChanged: true` capability advertised on
    /// `initialize` (parity 2.1.207 `J7n()`). Fire-and-forget and best-effort:
    /// a send failure is logged with the claude-code parity string and
    /// swallowed (`sendRootsListChanged()` → `MCP: failed to send
    /// roots/list_changed: ${err}`).
    pub fn send_roots_list_changed(&self) {
        if let Err(e) = self
            .connection
            .notify("notifications/roots/list_changed", serde_json::json!({}))
        {
            tracing::warn!(
                target: "lingxi_mcp::client",
                server = %self.server_name,
                "MCP: failed to send roots/list_changed: {e}",
            );
        }
    }

    /// Server name supplied at construction time. Used as the `<server>`
    /// component in the `mcp__<server>__<tool>` tool full-name format.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// claude-code's `((conn.transportErrorState?.pendingElicitations) ?? 0) > 0`
    /// — is an elicitation dialog open on this connection right now?
    ///
    /// The MCP auto-background race consults this at each deadline: while it is
    /// true the call stays in the FOREGROUND, because detaching a `tools/call`
    /// that is waiting on the user turns a question into an orphan (MON-08).
    ///
    /// The oracle's `type === "connected"` guard needs no analogue — holding an
    /// `Arc<McpClient>` already means a live registered client, and a lookup
    /// miss is the `?? 0` fallback.
    #[must_use]
    pub fn has_pending_elicitation(&self) -> bool {
        self.elicitation_pending.load(Ordering::Acquire) > 0
    }

    /// Send the MCP `initialize` request, parse the server capability
    /// declaration, and cache it on `self`. Also captures and truncates
    /// the optional `instructions` field per claude-code parity.
    ///
    /// Emits a JSON-RPC payload whose bytes contain:
    ///   * `"method":"initialize"`
    ///   * `"clientInfo":{"name":"lingxi", ...}`
    ///   * `"protocolVersion":"2025-11-25"`
    ///   * `"capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}}`
    ///     for the current ordinary transport default; an explicit Bare
    ///     initialize choice uses an empty elicitation object.
    ///
    /// On success, the parsed [`ServerCapabilitiesDto`] is both returned
    /// and stored in [`McpClient::server_capabilities`]; the optional
    /// `instructions` string is truncated (see [`truncate_description`])
    /// and stored in [`McpClient::server_instructions`].
    pub async fn initialize(&self) -> Result<ServerCapabilitiesDto, McpClientError> {
        let mut params = InitializeParams::default();
        params.capabilities.elicitation = self
            .elicitation
            .legacy
            .wire()
            .as_object()
            .expect("elicitation capability is an object")
            .clone();
        let resp: InitializeResponse = self
            .connection
            .call("initialize", &params)
            .await
            .map_err(|e| McpClientError::Rpc(e.to_string()))?;

        let caps = decode_server_capabilities(&resp.capabilities);
        *self.server_capabilities.write().await = Some(caps.clone());
        // TR-03: keep the undecoded object too — `capabilities.extensions` has
        // no DTO field and the MCP-skills directory-read predicate needs it.
        *self.raw_server_capabilities.write().await = Some(resp.capabilities.clone());

        // Truncate server instructions matching claude-code behavior
        // (client.ts:1163-1166: if > MAX_MCP_DESCRIPTION_LENGTH, slice
        // + "\u{2026} [truncated]").
        if let Some(raw) = resp.instructions {
            let orig_len = raw.encode_utf16().count();
            let limit = mcp_description_length_limit();
            let truncated = truncate_description_at_limit(&raw, limit).into_owned();
            if orig_len > limit {
                tracing::warn!(
                    target: "lingxi_mcp::client",
                    server = %self.server_name,
                    from = orig_len,
                    to = limit,
                    "Server instructions truncated from {orig_len} to {limit} UTF-16 units",
                );
            }
            *self.server_instructions.write().await = Some(truncated);
        }

        Ok(caps)
    }

    /// Returns the (possibly truncated) server instructions captured during
    /// `initialize`, or `None` if the server omitted them.
    pub async fn server_instructions(&self) -> Option<String> {
        self.server_instructions.read().await.clone()
    }

    /// Returns a clone of the server capabilities captured during
    /// `initialize`, or `None` if `initialize` has not yet completed.
    pub async fn server_capabilities(&self) -> Option<ServerCapabilitiesDto> {
        self.server_capabilities.read().await.clone()
    }

    async fn list_paginated<T>(
        &self,
        method: &'static str,
        decode: fn(serde_json::Value) -> Result<(Vec<T>, Option<String>), McpClientError>,
    ) -> Result<Vec<T>, McpClientError> {
        // Claude Code's modern `client.listTools()` path reports one aggregate
        // event, while legacy tools plus every prompts/resources traversal use
        // the page-level helper. The distinction is observable: aggregate
        // success is emitted even for one page and omits `pageCount`.
        let aggregate_tools = method == "tools/list" && self.modern();
        let mut emitted_error = false;
        let mut retry_index = 0_usize;
        'retry: loop {
            let mut items = Vec::new();
            let mut cursor: Option<String> = None;
            let mut page_count = 0_usize;
            loop {
                let mut params = serde_json::json!({});
                if let Some(cursor) = cursor.as_deref() {
                    params["cursor"] = serde_json::Value::String(cursor.to_string());
                }
                let raw_value: serde_json::Value = match self.request_result(method, params).await {
                    Ok(value) => value,
                    Err(error) => {
                        if !aggregate_tools && page_count > 0 && !emitted_error {
                            emit_list_paginated(
                                method,
                                Some(page_count),
                                items.len(),
                                "error",
                                Some("pages"),
                            );
                            emitted_error = true;
                        }
                        if matches!(&error, McpResultError::Connection(rpc) if retryable_list_connection_error(rpc))
                        {
                            if let Some(delay) = MCP_LIST_RETRY_DELAYS.get(retry_index).copied() {
                                retry_index += 1;
                                tracing::debug!(
                                    target: "lingxi_mcp::client",
                                    server = %self.server_name,
                                    method,
                                    delay_ms = delay.as_millis(),
                                    "{method} failed ({error}); retrying",
                                );
                                tokio::time::sleep(delay).await;
                                continue 'retry;
                            }
                        }
                        return Err(mcp_client_error_from_result(error));
                    }
                };

                // The oracle increments pageCount immediately after a response
                // arrives, before schema decoding. A malformed second page is
                // therefore reported as pageCount=2 with only page-one items.
                page_count += 1;
                let (page_items, next_cursor) = match decode(raw_value) {
                    Ok(page) => page,
                    Err(error) => {
                        if !aggregate_tools && !emitted_error {
                            emit_list_paginated(
                                method,
                                Some(page_count),
                                items.len(),
                                "error",
                                Some("pages"),
                            );
                        }
                        return Err(error);
                    }
                };
                items.extend(page_items);
                cursor = next_cursor;

                let capped = cursor.is_some() && page_count >= MAX_MCP_LIST_PAGES;
                if cursor.is_none() || capped {
                    if aggregate_tools {
                        if capped {
                            emit_list_paginated(method, None, 0, "capped", Some("aggregate"));
                            return Err(McpClientError::Rpc(format!(
                                "{method} pagination exceeded {MAX_MCP_LIST_PAGES} pages"
                            )));
                        }
                        emit_list_paginated(
                            method,
                            None,
                            items.len(),
                            "complete",
                            Some("aggregate"),
                        );
                        return Ok(items);
                    }
                    // Ordinary one-page success is intentionally silent. The
                    // oracle reports this `pages` source only after traversing
                    // multiple pages; its aggregate tools helper is separate.
                    if page_count > 1 {
                        emit_list_paginated(
                            method,
                            Some(page_count),
                            items.len(),
                            if capped { "capped" } else { "complete" },
                            Some("pages"),
                        );
                    }
                    if capped {
                        tracing::warn!(
                            target: "lingxi_mcp::client",
                            server = %self.server_name,
                            "{method}: stopped at {} pages with more pending",
                            MAX_MCP_LIST_PAGES,
                        );
                    }
                    return Ok(items);
                }
            }
        }
    }

    /// Enumerate every tool advertised by the server.
    ///
    /// Sends `tools/list`, applies Unicode sanitization (see
    /// [`recursively_sanitize_unicode`]) to the raw response to mitigate
    /// ASCII Smuggling / Hidden Prompt Injection (HackerOne #3086545, mirrors
    /// `client.ts:1758`: `recursivelySanitizeUnicode(result.tools)`), then
    /// decorates each entry with the normalized full-name
    /// `mcp__<server>__<tool>` and truncates oversized descriptions through
    /// [`truncate_description`].
    ///
    /// Also checks `CLAUDE_AGENT_SDK_MCP_NO_PREFIX`: when set (truthy) and the
    /// transport is an SDK-embedded server (`sdk` type), the `mcp__` prefix is
    /// omitted so MCP tools can override builtins by name. Mirrors
    /// `client.ts:1762-1770`.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDto>, McpClientError> {
        let raw_tools = self
            .list_paginated("tools/list", decode_tools_list_page)
            .await?;

        // Mirror `client.ts:1762-1770`: `CLAUDE_AGENT_SDK_MCP_NO_PREFIX` env gate.
        // When this env var is truthy AND the transport type is "sdk", the tool
        // name is sent to the model without the `mcp__` prefix (allowing SDK-
        // embedded MCP tools to shadow builtins by their raw name).
        let skip_prefix = self.skip_mcp_prefix();

        let normalized_server = crate::normalization::normalize_name_for_mcp(&self.server_name);
        let mut tools: Vec<McpToolDto> = raw_tools
            .into_iter()
            .filter_map(|t| {
                // §20a — normalize or drop the tool's `inputSchema` before it
                // reaches the model (oracle `Wrt`/`qrt`, see
                // `crate::tool_schema`). Must run before the DTO is built so
                // a dropped tool never gets constructed.
                let decision =
                    crate::tool_schema::decide_tool_schema(self.server_url.as_deref(), &t.input_schema);
                if let Some(reason) = decision.drop_reason {
                    tracing::warn!(
                        server = %self.server_name,
                        tool = %t.name,
                        "Skipping tool \"{}\": {reason}. Other tools from this server remain available.",
                        t.name
                    );
                    return None;
                }
                if let Some(warning) = &decision.warning {
                    tracing::debug!(
                        server = %self.server_name,
                        tool = %t.name,
                        "Tool \"{}\" {warning}",
                        t.name
                    );
                }

                // Normalize BOTH segments (server + tool) — 1:1 with TS
                // `buildMcpToolName` = `getMcpPrefix(server) +
                // normalizeNameForMCP(toolName)` (`mcpStringUtils.ts:51`).
                let norm_tool = crate::normalization::normalize_name_for_mcp(&t.name);
                let full_name = if skip_prefix {
                    t.name.clone()
                } else {
                    format!("mcp__{normalized_server}__{norm_tool}")
                };
                let description = match decision.description_note {
                    // oracle: `E.description ? \`${note}\n\n${description}\` : note`.
                    Some(note) if !t.description.is_empty() => {
                        format!("{note}\n\n{}", t.description)
                    }
                    Some(note) => note,
                    None => t.description,
                };
                let search_hint = tool_meta_search_hint(&t.meta);
                let always_load = tool_meta_always_load(&t.meta);
                let requires_user_interaction = tool_meta_requires_user_interaction(&t.meta);
                Some(McpToolDto {
                    full_name,
                    server_name: self.server_name.clone(),
                    description: truncate_description(&description).into_owned(),
                    input_schema: decision.schema,
                    output_schema: t.output_schema,
                    annotations: t.annotations,
                    icons: t.icons,
                    meta: t.meta,
                    tool_name: t.name,
                    // Forward `_meta.anthropic/searchHint` + `alwaysLoad`
                    // from the wire (client.ts:1777-1780). Both default
                    // to `None` for servers that omit `_meta`. A server-level
                    // `alwaysLoad: true` config (parity 2.1.207 P2-01) forces
                    // ALL of this server's tools always-loaded, overriding an
                    // absent per-tool bit.
                    search_hint,
                    always_load: if self.config_always_load {
                        Some(true)
                    } else {
                        always_load
                    },
                    requires_user_interaction,
                })
            })
            .collect();
        self.apply_config_permission_ceilings(&mut tools);
        Ok(tools)
    }

    /// Resolve config-side ceilings against the raw names returned by
    /// `tools/list`. The effective result is kept in a side map so existing
    /// `McpToolDto` and discovery-cache payloads remain byte-compatible.
    fn apply_config_permission_ceilings(&self, tools: &mut [McpToolDto]) {
        let mut org_ceilings = BTreeMap::<&str, McpPermissionCeiling>::new();
        for tool in &self.config_tools {
            if let Some(ceiling) = tool.org_max_permission {
                org_ceilings
                    .entry(tool.name.as_str())
                    .and_modify(|current| *current = current.strictest(ceiling))
                    .or_insert(ceiling);
            }
            // A server-declared always-ask/deny policy is also a tighten-only
            // ceiling for this side-map. `always_allow` contributes the
            // neutral `allow` value, preserving strictest-wins semantics when
            // an org ceiling is present for the same raw tool name.
            if let Some(policy) = tool.permission_policy {
                let policy_ceiling = match policy {
                    lingxi_core::host::McpToolPermissionPolicy::AlwaysAllow => {
                        McpPermissionCeiling::Allow
                    }
                    lingxi_core::host::McpToolPermissionPolicy::AlwaysAsk => {
                        McpPermissionCeiling::Ask
                    }
                    lingxi_core::host::McpToolPermissionPolicy::AlwaysDeny => {
                        McpPermissionCeiling::Deny
                    }
                };
                org_ceilings
                    .entry(tool.name.as_str())
                    .and_modify(|current| *current = current.strictest(policy_ceiling))
                    .or_insert(policy_ceiling);
            }
        }

        let mut effective = BTreeMap::new();
        let mut matched_tool_permissions = BTreeSet::new();
        for tool in tools {
            let tool_name = tool.tool_name.clone();
            let mut ceiling = self.config_tool_permissions.get(&tool_name).copied();
            if ceiling.is_some() {
                matched_tool_permissions.insert(tool_name.clone());
            }
            if let Some(org) = org_ceilings.get(tool_name.as_str()).copied() {
                ceiling = Some(ceiling.map_or(org, |current| current.strictest(org)));
            }
            if let Some(ceiling) = ceiling {
                effective.insert(tool_name, ceiling);
            }
        }
        *self
            .effective_permission_ceilings
            .write()
            .unwrap_or_else(|error| error.into_inner()) = effective;

        if !self.config_tool_permissions.is_empty() && matched_tool_permissions.is_empty() {
            tracing::warn!(
                server = %self.server_name,
                "toolPermissions has {} entries but none matched upstream tool names — backend name drift?",
                self.config_tool_permissions.len()
            );
        }
    }

    /// Read the strictest config ceiling for a raw upstream tool name after
    /// `list_tools` has run. `None` means no ceiling was configured or the
    /// tool was not advertised by the server.
    #[must_use]
    pub fn effective_max_permission(&self, tool_name: &str) -> Option<McpPermissionCeiling> {
        self.effective_permission_ceilings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(tool_name)
            .copied()
    }

    /// Returns `true` when the `CLAUDE_AGENT_SDK_MCP_NO_PREFIX` env var is set
    /// to a truthy value (mirrors `isEnvTruthy` in TS). The transport-type
    /// check (`type === 'sdk'`) is omitted here because LingXi's `McpClient`
    /// does not carry the config; callers in the SDK-embedded path set the env.
    fn skip_mcp_prefix(&self) -> bool {
        is_env_truthy("CLAUDE_AGENT_SDK_MCP_NO_PREFIX")
    }

    /// Invoke a tool by its `mcp__<server>__<tool>` full-name with the resolved
    /// per-call timeout ([`mcp_tool_timeout_for`]: the per-server config
    /// `timeout` when `>= 1000ms`, else the `MCP_TOOL_TIMEOUT` env var, else the
    /// ~27.8h default; clamped to `[1000, i32::MAX]`ms).
    pub async fn call_tool(
        &self,
        full_name: &str,
        input: serde_json::Value,
    ) -> Result<McpToolResultDto, McpClientError> {
        self.call_tool_with_timeout(
            full_name,
            input,
            mcp_tool_timeout_for(self.config_timeout_ms),
        )
        .await
    }

    /// `call_tool` with a custom timeout — used by tests to exercise the
    /// timeout branch without waiting the full default. Carries no request
    /// `_meta` and no progress wiring (delegates to [`Self::call_tool_with_meta`]
    /// with `tool_use_id = None`, `on_progress = None`).
    pub async fn call_tool_with_timeout(
        &self,
        full_name: &str,
        input: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<McpToolResultDto, McpClientError> {
        self.call_tool_with_meta(full_name, input, timeout, None, None)
            .await
    }

    /// [`Self::call_tool`] that also threads the model's `toolUseId` into the
    /// request `_meta` (the byte-exact `claudecode/toolUseId` key) and forwards
    /// MCP `notifications/progress` to `on_progress`. Resolves the per-call
    /// timeout exactly like [`Self::call_tool`]
    /// ([`mcp_tool_timeout`]). Mirrors claude-code's per-tool `call`
    /// (`services/mcp/client.ts:1833-1881` + `:3029-3116`).
    pub async fn call_tool_with_progress(
        &self,
        full_name: &str,
        input: serde_json::Value,
        tool_use_id: Option<&str>,
        on_progress: Option<McpProgressCallback>,
    ) -> Result<McpToolResultDto, McpClientError> {
        self.call_tool_with_meta(
            full_name,
            input,
            mcp_tool_timeout_for(self.config_timeout_ms),
            tool_use_id,
            on_progress,
        )
        .await
    }

    /// Core `tools/call` path with the optional `_meta` / progress wiring.
    ///
    /// Strips the `mcp__<server>__` prefix from `full_name` to recover the
    /// unprefixed wire `name`. On timeout produces
    /// [`McpClientError::Timeout`] with its locked Display format.
    ///
    /// MCP.3: when `tool_use_id` is `Some`, the request gains
    /// `_meta: { "claudecode/toolUseId": <id> }` (byte-exact key, mirroring
    /// `client.ts:1840-1843` building `meta` and `:3096` forwarding it as
    /// `_meta` on `callTool`).
    ///
    /// MCP.4: when `on_progress` is `Some` AND a `tool_use_id` is present
    /// (mirroring the `onProgress && toolUseId` gate at `client.ts:1846`/`:1871`),
    /// the request `_meta` additionally carries a `progressToken` and a
    /// forwarder task surfaces each matching inbound `notifications/progress`
    /// (`{ progress, total, message }`) to the callback — mirroring how the SDK
    /// registers an `onprogress` handler keyed by the request's progressToken
    /// (`client.ts:3102-3114`).
    pub async fn call_tool_with_meta(
        &self,
        full_name: &str,
        input: serde_json::Value,
        timeout: std::time::Duration,
        tool_use_id: Option<&str>,
        on_progress: Option<McpProgressCallback>,
    ) -> Result<McpToolResultDto, McpClientError> {
        // Strip the mcp__<server>__ prefix to recover the wire `name`. The
        // server token is normalized to match how `list_tools` built the FQN
        // (so the round-trip is self-consistent for invalid-char names).
        let prefix = format!(
            "mcp__{}__",
            crate::normalization::normalize_name_for_mcp(&self.server_name)
        );
        let tool_name = full_name
            .strip_prefix(&prefix)
            .ok_or_else(|| {
                McpClientError::Rpc(format!("full_name {full_name:?} missing prefix {prefix:?}"))
            })?
            .to_string();

        if self.modern() {
            return self
                .call_modern_tool(&tool_name, input, timeout, tool_use_id, on_progress)
                .await;
        }

        // MCP.3: assemble the request `_meta`. claude-code stamps
        // `_meta: { "claudecode/toolUseId": <id> }` onto the tools/call request
        // (`client.ts:1840-1843` builds `meta`; `:3096` forwards it as `_meta`).
        let mut meta = serde_json::Map::new();
        if let Some(id) = tool_use_id {
            meta.insert(
                "claudecode/toolUseId".to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }

        // P2-01 (`GLd`): the per-call idle/silence timeout. A stdio server may
        // stay silent (no response, no progress) for at most 30 min, a remote one
        // 5 min, before the watchdog aborts — capped by the overall `BHs` ceiling
        // and disabled (`ZERO`) for the in-process transports. The idle window is
        // reset by inbound `notifications/progress` (claude-code onprogress:
        // `v = Date.now()`).
        let idle_timeout = mcp_tool_idle_timeout_for(self.config_timeout_ms, self.transport_kind);
        let watchdog_active = !idle_timeout.is_zero();
        // Last-activity instant shared with the progress listener; the watchdog
        // measures silence against it. Seeded at "now" (the call is about to fly).
        // On the tokio clock so `sleep`-driven watchdog checks and `elapsed`
        // measurements agree (and paused-time tests are deterministic).
        let last_activity = Arc::new(std::sync::Mutex::new(tokio::time::Instant::now()));

        // MCP.4: subscribe to inbound notifications BEFORE the request is sent
        // (so no early `notifications/progress` is missed) and spawn a forwarder
        // matching by the minted `progressToken`. The `progressToken` is minted
        // (and stamped into `_meta`, altering the outgoing request body) ONLY when
        // both a callback and a toolUseId exist (the `onProgress && toolUseId`
        // gate — outgoing request-body parity is locked to that surface). When a
        // token IS stamped, the listener also resets the idle watchdog on every
        // matching progress note.
        let progress_active = on_progress.is_some() && tool_use_id.is_some();
        let forwarder = if progress_active {
            // The MCP SDK uses the outgoing request id as the progressToken; the
            // router owns request ids here, so we mint a process-unique token and
            // stamp it into `_meta.progressToken` for the server to echo back.
            let token = serde_json::Value::String(format!(
                "lingxi-mcp-progress-{}",
                NEXT_PROGRESS_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            meta.insert("progressToken".to_string(), token.clone());
            let callback = on_progress.expect("progress_active implies Some(callback)");
            let mut notifications = self.connection.notifications();
            let activity = last_activity.clone();
            Some(tokio::spawn(async move {
                loop {
                    match notifications.recv().await {
                        Ok(n) => {
                            if n.method != "notifications/progress" {
                                continue;
                            }
                            let Some(p) = n.params.as_ref() else {
                                continue;
                            };
                            if p.get("progressToken") != Some(&token) {
                                continue;
                            }
                            // Reset the idle watchdog: a matching progress note is
                            // liveness (claude-code onprogress: `v = Date.now()`).
                            if let Ok(mut a) = activity.lock() {
                                *a = tokio::time::Instant::now();
                            }
                            // SDK `onprogress` payload: `{progress, total?, message?}`
                            // (`client.ts:3109-3111`).
                            let progress = p
                                .get("progress")
                                .and_then(serde_json::Value::as_f64)
                                .unwrap_or(0.0);
                            let total = p.get("total").and_then(serde_json::Value::as_f64);
                            let message = p
                                .get("message")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string);
                            callback(McpProgressEvent {
                                progress,
                                total,
                                message,
                            });
                        }
                        // Fell behind the broadcast buffer — keep listening.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        // Connection's notification stream ended.
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }))
        } else {
            None
        };

        let mut params = serde_json::json!({
            "name": tool_name,
            "arguments": input,
        });
        if !meta.is_empty() {
            if let Some(obj) = params.as_object_mut() {
                obj.insert("_meta".to_string(), serde_json::Value::Object(meta));
            }
        }

        // Sub-second timeouts still report at least 1s to keep the
        // user-facing error string stable. Round up using ceil semantics on
        // the millis fraction so non-zero sub-second timeouts don't collapse
        // to "after 0s".
        let secs = timeout.as_secs().max(1);
        let params = self.request_params("tools/call", params);
        let fut = self
            .connection
            .call::<_, serde_json::Value>("tools/call", params);

        // Race the call against the overall `BHs` timeout and — when enabled —
        // the `GLd` idle watchdog. When the idle watchdog is disabled (`ZERO`)
        // the single-timeout path is byte-identical to before.
        let outcome = if watchdog_active {
            let server = self.server_name.clone();
            let idle_tool = tool_name.clone();
            tokio::select! {
                biased;
                r = tokio::time::timeout(timeout, fut) => match r {
                    Err(_elapsed) => Err(McpClientError::Timeout {
                        server: self.server_name.clone(),
                        tool: tool_name,
                        secs,
                    }),
                    Ok(Err(e)) => Err(mcp_client_error_from_rpc(&e)),
                    Ok(Ok(value)) => {
                        serde_json::from_value::<ToolCallResponse>(value)
                                .map(|resp| McpToolResultDto {
                                    content: resp.content,
                                    is_error: resp.is_error,
                                    meta: resp.meta,
                                    structured_content: resp.structured_content,
                                })
                                .map_err(|e| McpClientError::Deserialize(e.to_string()))
                    },
                },
                idle = idle_watchdog(idle_timeout, last_activity.clone(), server, idle_tool) => idle,
            }
        } else {
            match tokio::time::timeout(timeout, fut).await {
                Err(_elapsed) => Err(McpClientError::Timeout {
                    server: self.server_name.clone(),
                    tool: tool_name,
                    secs,
                }),
                Ok(Err(e)) => Err(mcp_client_error_from_rpc(&e)),
                Ok(Ok(value)) => serde_json::from_value::<ToolCallResponse>(value)
                    .map(|resp| McpToolResultDto {
                        content: resp.content,
                        is_error: resp.is_error,
                        meta: resp.meta,
                        structured_content: resp.structured_content,
                    })
                    .map_err(|e| McpClientError::Deserialize(e.to_string())),
            }
        };

        // The call settled — stop forwarding progress for this request.
        if let Some(handle) = forwarder {
            handle.abort();
        }

        outcome
    }

    /// Enumerate every prompt advertised by the server.
    ///
    /// Sends `prompts/list`, applies Unicode sanitization (mirrors
    /// `client.ts:2051`: `recursivelySanitizeUnicode(result.prompts)`), then
    /// truncates oversized prompt descriptions through [`truncate_description`].
    pub async fn list_prompts(&self) -> Result<Vec<McpPromptDto>, McpClientError> {
        let raw_prompts = self
            .list_paginated("prompts/list", decode_prompts_list_page)
            .await?;

        Ok(raw_prompts
            .into_iter()
            .map(|p| McpPromptDto {
                name: p.name,
                description: p.description.map(|d| truncate_description(&d).into_owned()),
                arguments: p
                    .arguments
                    .into_iter()
                    .map(|argument| lingxi_core::host::McpPromptArgumentDto {
                        name: argument.name,
                        description: argument.description,
                        required: argument.required,
                    })
                    .collect(),
            })
            .collect())
    }

    /// Render a prompt template with the supplied arguments. The returned
    /// `Value` is the server's `{description, messages}` envelope verbatim.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpClientError> {
        let params = serde_json::json!({ "name": name, "arguments": arguments });
        let value: serde_json::Value = self
            .request_result("prompts/get", params)
            .await
            .map_err(mcp_client_error_from_result)?;
        Ok(value)
    }

    /// Enumerate every resource advertised by the server.
    ///
    /// Sends `resources/list` and returns the parsed entries verbatim. The
    /// MCP spec lets `mimeType` be absent for opaque/unknown content; we
    /// surface that as `None`.
    pub async fn list_resources(&self) -> Result<Vec<McpResourceDto>, McpClientError> {
        let raw_resources = self
            .list_paginated("resources/list", decode_resources_list_page)
            .await?;
        Ok(raw_resources
            .into_iter()
            .map(|r| McpResourceDto {
                uri: r.uri,
                name: r.name,
                description: r.description,
                mime_type: r.mime_type,
                meta: r.meta,
            })
            .collect())
    }

    /// Fetch the contents of one resource by URI. Returns the FIRST element
    /// of the server's `contents` array (the protocol allows multiple but
    /// claude-code always reads the first).
    pub async fn read_resource(&self, uri: &str) -> Result<McpResourceContentDto, McpClientError> {
        let raw_value: serde_json::Value = self
            .request_result("resources/read", serde_json::json!({ "uri": uri }))
            .await
            .map_err(mcp_client_error_from_result)?;
        let resp: ResourceReadResponse = serde_json::from_value(raw_value)
            .map_err(|e| McpClientError::Deserialize(e.to_string()))?;
        resp.contents
            .into_iter()
            .next()
            .map(|c| McpResourceContentDto {
                uri: c.uri,
                content: c.text,
                mime_type: c.mime_type,
                meta: c.meta,
            })
            .ok_or_else(|| {
                McpClientError::Deserialize("resources/read returned empty contents array".into())
            })
    }

    /// Fetch the FULL multi-content `contents[]` array of a resource (MCP-5d).
    ///
    /// Unlike [`Self::read_resource`] (which collapses to the first text block
    /// and drops `mimeType`/blob), this returns every content block with its
    /// `mimeType`, distinguishes text from base64 blobs, decodes blobs, and
    /// persists their bytes under `output_dir` — returning `blobSavedTo` paths.
    /// 1:1 with `ReadMcpResourceTool.ts:95-139`.
    ///
    /// `server_name` is used to build the `"[Resource from <server> at <uri>] "`
    /// prefix of the persisted-blob message. `output_dir` is the directory blob
    /// bytes are written to (a session/tool-results dir in production).
    pub async fn read_resource_rich(
        &self,
        uri: &str,
        output_dir: &std::path::Path,
    ) -> Result<Vec<lingxi_core::host::McpResourceContentsRich>, McpClientError> {
        let raw_value: serde_json::Value = self
            .request_result("resources/read", serde_json::json!({ "uri": uri }))
            .await
            .map_err(mcp_client_error_from_result)?;
        let resp: ResourceReadRichResponse = serde_json::from_value(raw_value)
            .map_err(|e| McpClientError::Deserialize(e.to_string()))?;
        let raw: Vec<crate::mcp_output_storage::RawResourceContent> = resp
            .contents
            .into_iter()
            .map(|c| crate::mcp_output_storage::RawResourceContent {
                // Echo the request URI when the server omits one (parity with
                // the posix transport).
                uri: c.uri.unwrap_or_else(|| uri.to_string()),
                mime_type: c.mime_type,
                meta: c.meta,
                text: c.text,
                blob: c.blob,
            })
            .collect();
        let (now_millis, rand_tag) = persist_id_seed();
        Ok(crate::mcp_output_storage::map_resource_contents(
            raw,
            &self.server_name,
            output_dir,
            now_millis,
            &rand_tag,
        ))
    }

    /// The RAW `capabilities` object captured during `initialize`, or `None`
    /// if `initialize` has not completed. See [`Self::raw_server_capabilities`]
    /// — the decoded [`ServerCapabilitiesDto`] drops `extensions`, which the
    /// MCP-skills directory-read predicate needs.
    pub async fn server_capabilities_raw(&self) -> Option<serde_json::Value> {
        self.raw_server_capabilities.read().await.clone()
    }

    /// Paginated `resources/directory/read` — the port of 2.1.238's
    /// `readMcpDirectory` (`Cpv`, oracle @289723766):
    ///
    /// ```js
    /// async function Cpv(e,t){
    ///   if(!FSf(e.capabilities))throw Error("readMcpDirectory called on a server without directoryRead capability");
    ///   let r=[],n,o=0;
    ///   do{ let i;
    ///       try{ i=await ug(e.client).request({method:"resources/directory/read",params:{uri:t,...n&&{cursor:n}}},F5e,{timeout:uC()}) }
    ///       catch(s){ if(o===0||!(s instanceof Jy&&s.code===Qp.InvalidParams))throw s;
    ///                 return It(e.name,`resources/directory/read ${t}: page ${o+1} returned InvalidParams on cursor; returning ${r.length} entries from prior pages`),r }
    ///       r.push(...i.resources),n=i.nextCursor,o++
    ///   }while(n&&o<NSf);
    ///   if(n)It(e.name,`resources/directory/read ${t}: stopped at ${NSf} pages with more pending`);
    ///   return r}
    /// var NSf=20;
    /// ```
    ///
    /// Three details are load-bearing and preserved verbatim:
    /// * `cursor` is OMITTED from `params` on the first page (`...n&&{cursor:n}`),
    ///   not sent as `null`.
    /// * An `InvalidParams` (-32602) failure is tolerated ONLY after page 1 —
    ///   it means the server rejected the cursor, and the pages already
    ///   collected are returned instead of erroring. Any other code, or an
    ///   InvalidParams on the FIRST page, propagates.
    /// * The page loop is hard-capped at [`MAX_MCP_DIRECTORY_PAGES`]; a cursor
    ///   still pending at the cap is logged and the partial listing returned.
    ///
    /// The capability precondition is the CALLER's (`ReadMcpResourceDirTool`
    /// checks `serverDeclaresDirectoryRead` and returns the model-facing
    /// "does not support directory listing." message before reaching here), so
    /// this method does not re-derive it.
    pub async fn read_mcp_directory(
        &self,
        uri: &str,
    ) -> Result<Vec<McpDirectoryEntry>, McpClientError> {
        let mut out: Vec<McpDirectoryEntry> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page: usize = 0;
        loop {
            let mut params = serde_json::json!({ "uri": uri });
            if let Some(c) = cursor.as_deref() {
                params["cursor"] = serde_json::Value::String(c.to_string());
            }
            let raw_value: serde_json::Value = match self
                .request_result("resources/directory/read", params)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if page > 0
                        && matches!(&e, McpResultError::Connection(rpc) if is_invalid_params(rpc))
                    {
                        tracing::warn!(
                            target: "lingxi_mcp::client",
                            server = %self.server_name,
                            "resources/directory/read {uri}: page {} returned InvalidParams on cursor; returning {} entries from prior pages",
                            page + 1,
                            out.len(),
                        );
                        return Ok(out);
                    }
                    return Err(McpClientError::Rpc(e.to_string()));
                }
            };
            let resp: DirectoryReadResponse = serde_json::from_value(raw_value)
                .map_err(|e| McpClientError::Deserialize(e.to_string()))?;
            out.extend(resp.resources);
            cursor = resp.next_cursor;
            page += 1;
            if cursor.is_none() || page >= MAX_MCP_DIRECTORY_PAGES {
                break;
            }
        }
        if cursor.is_some() {
            tracing::warn!(
                target: "lingxi_mcp::client",
                server = %self.server_name,
                "resources/directory/read {uri}: stopped at {} pages with more pending",
                MAX_MCP_DIRECTORY_PAGES,
            );
        }
        Ok(out)
    }

    /// Liveness probe — JSON-RPC `ping` with no params; success on any
    /// non-error response. The health checker uses this to detect dead
    /// servers without forcing a full `tools/list` roundtrip.
    pub async fn ping(&self) -> Result<(), McpClientError> {
        let _: serde_json::Value = self
            .connection
            .call("ping", serde_json::Value::Null)
            .await
            .map_err(|e| McpClientError::Rpc(e.to_string()))?;
        Ok(())
    }
}

/// Monotonic source for per-call MCP `progressToken`s (MCP.4). The MCP SDK
/// reuses the outgoing request id as the progressToken; the router owns request
/// ids here, so we mint a process-unique token and stamp it into the request
/// `_meta.progressToken` so inbound `notifications/progress` can be matched back
/// to the originating `tools/call`.
static NEXT_PROGRESS_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One MCP progress update forwarded from a `notifications/progress` received
/// during an in-flight `tools/call` (MCP.4). Mirrors the SDK `onprogress`
/// payload (`{ progress, total, message }`, `services/mcp/client.ts:3109-3111`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpProgressEvent {
    /// Monotonic progress amount reported by the server.
    pub progress: f64,
    /// Optional total against which `progress` advances.
    pub total: Option<f64>,
    /// Optional human-readable status message.
    pub message: Option<String>,
}

/// Callback invoked for each forwarded MCP progress notification (MCP.4).
/// Cloneable and `Send + Sync` so it can be moved into the broadcast-forwarder
/// task that [`McpClient::call_tool_with_meta`] spawns.
pub type McpProgressCallback = Arc<dyn Fn(McpProgressEvent) + Send + Sync>;

/// Wire-level shape of a `prompts/list` response body.
#[derive(Debug, Deserialize)]
struct PromptsListResponse {
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
    #[serde(default)]
    prompts: Vec<RawPrompt>,
}

/// Wire-level shape for one prompt entry inside `prompts/list`.
#[derive(Debug, Deserialize)]
struct RawPrompt {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    arguments: Vec<RawPromptArgument>,
}

/// Wire-level shape for one prompt argument.
#[derive(Debug, Deserialize)]
struct RawPromptArgument {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    required: bool,
}

/// Wire-level shape of a `resources/list` response body.
#[derive(Debug, Deserialize)]
struct ResourcesListResponse {
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
    #[serde(default)]
    resources: Vec<RawResource>,
}

/// Hard cap on paginated ordinary list methods (`tools/list`, `prompts/list`,
/// `resources/list`). A pending cursor past this point logs and returns the
/// partial aggregate rather than looping forever on a bad server.
pub const MAX_MCP_LIST_PAGES: usize = 20;

/// Wire-level shape for one resource entry inside `resources/list`.
#[derive(Debug, Deserialize)]
struct RawResource {
    uri: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "_meta", default)]
    meta: Option<serde_json::Value>,
}

/// TR-03 `NSf` (oracle @289724681) — the hard page cap on a paginated
/// `resources/directory/read`. A cursor still pending at this page count is
/// logged and the partial listing returned.
pub const MAX_MCP_DIRECTORY_PAGES: usize = 20;

/// One direct child of a directory resource, as returned by
/// `resources/directory/read` (oracle schema `F5e = H5e.extend({resources:$d(CQe)})`,
/// @287151819 — the paginated-result base plus a resource array).
///
/// Subdirectories are distinguished by `mime_type == "inode/directory"`
/// (oracle `JBn`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpDirectoryEntry {
    /// Child resource URI — the value to pass back as `uri` to descend.
    pub uri: String,
    /// Child resource name.
    #[serde(default)]
    pub name: String,
    /// Child MIME type; absent when the server omits it (the oracle maps
    /// `c.mimeType!==void 0 ? qG(c.mimeType) : void 0`, i.e. an omitted type
    /// stays omitted rather than becoming `""`).
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Wire-level shape of one `resources/directory/read` page.
#[derive(Debug, Deserialize)]
struct DirectoryReadResponse {
    #[serde(default)]
    resources: Vec<McpDirectoryEntry>,
    /// Opaque continuation token; `None`/absent ends the pagination loop.
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
}

/// `s instanceof Jy && s.code === Qp.InvalidParams` — the oracle's tolerated
/// mid-pagination failure. JSON-RPC `InvalidParams` is -32602.
fn is_invalid_params(e: &jsonrpc::ConnectionError) -> bool {
    matches!(
        e,
        jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(r))
            if r.code == JSONRPC_INVALID_PARAMS
    )
}

/// JSON-RPC reserved error code for `InvalidParams` (`Qp.InvalidParams`).
const JSONRPC_INVALID_PARAMS: i32 = -32602;

/// Wire-level shape of a `resources/read` response body.
#[derive(Debug, Deserialize)]
struct ResourceReadResponse {
    contents: Vec<RawResourceContent>,
}

/// Wire-level shape for one element of the `contents` array in `resources/read`.
#[derive(Debug, Deserialize)]
struct RawResourceContent {
    uri: String,
    #[serde(default)]
    text: String,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "_meta", default)]
    meta: Option<serde_json::Value>,
}

/// Wire-level shape of a `resources/read` response for the MCP-5d rich path.
/// Separate from [`ResourceReadResponse`] so the legacy single-content path is
/// untouched: here every field is optional so a text block, a blob block, or an
/// opaque block all deserialize.
#[derive(Debug, Deserialize)]
struct ResourceReadRichResponse {
    #[serde(default)]
    contents: Vec<RawResourceContentRich>,
}

/// Wire-level shape for one `contents[]` element on the rich path.
#[derive(Debug, Deserialize)]
struct RawResourceContentRich {
    #[serde(default)]
    uri: Option<String>,
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(rename = "_meta", default)]
    meta: Option<serde_json::Value>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    blob: Option<String>,
}

/// Produce the `(now_millis, rand_tag)` seed for a blob `persistId`, mirroring
/// the TS `Date.now()` + `Math.random().toString(36).slice(2, 8)` pair
/// (`ReadMcpResourceTool.ts:114`). The exact value is non-load-bearing (it only
/// has to be unique per block); only the *template shape* is locked.
fn persist_id_seed() -> (u128, String) {
    // 6 lowercase-alphanumeric chars derived from a coarse nanosecond mix —
    // avoids pulling in an RNG crate while staying collision-resistant enough
    // for per-block filenames.
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let mut tag = String::with_capacity(6);
    for _ in 0..6 {
        tag.push(ALPHABET[(seed % 36) as usize] as char);
        seed /= 36;
    }
    (now_millis, tag)
}

/// Default per-call timeout for `tools/call` — 1:1 with claude-code's
/// `DEFAULT_MCP_TOOL_TIMEOUT_MS = 100_000_000` (~27.8h, "effectively infinite";
/// `client.ts:208-211`). Overridable per call via the `MCP_TOOL_TIMEOUT` env
/// var; see [`mcp_tool_timeout`]. (The previous 60s value was a fidelity bug:
/// it spuriously timed out legitimately long-running MCP tools that claude-code
/// lets run.)
pub const DEFAULT_CALL_TOOL_TIMEOUT_MS: u64 = 100_000_000;

/// [`DEFAULT_CALL_TOOL_TIMEOUT_MS`] as a [`std::time::Duration`].
pub const DEFAULT_CALL_TOOL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(DEFAULT_CALL_TOOL_TIMEOUT_MS);

/// Lower clamp for a resolved `tools/call` timeout — `Math.max(n, 1000)` in
/// claude-code's `BHs`.
const MCP_TOOL_TIMEOUT_MIN_MS: u64 = 1_000;
/// Upper clamp for a resolved `tools/call` timeout — claude-code's `WLd`
/// (`2_147_483_647`, i.e. `i32::MAX` ms, the largest value `setTimeout`
/// accepts without truncation).
const MCP_TOOL_TIMEOUT_MAX_MS: u64 = 2_147_483_647;

/// Resolve the per-call `tools/call` timeout with NO per-server config value —
/// equivalent to `mcp_tool_timeout_for(None)`. Used by call sites that lack a
/// resolved server config (e.g. the low-level platform transport).
#[must_use]
pub fn mcp_tool_timeout() -> std::time::Duration {
    mcp_tool_timeout_for(None)
}

/// Resolve the per-call `tools/call` timeout, honouring an optional per-server
/// config `timeout` — a 1:1 port of claude-code `BHs` (`client.ts`):
///
/// ```js
/// function BHs(e){
///   let t=parseInt(process.env.MCP_TOOL_TIMEOUT||"",10),
///       n=(e?.timeout!==void 0 && e.timeout>=1000 ? e.timeout : void 0)
///         ?? (t>0 ? t : void 0) ?? 1e8;
///   return Math.min(Math.max(n,1000), 2147483647)
/// }
/// ```
///
/// Precedence: the config timeout wins **only when `>= 1000ms`**; otherwise the
/// `MCP_TOOL_TIMEOUT` env var (when it parses `> 0`); otherwise the
/// [`DEFAULT_CALL_TOOL_TIMEOUT_MS`] (`1e8`) default. The chosen value is then
/// clamped to `[1000, 2_147_483_647]` ms.
#[must_use]
pub fn mcp_tool_timeout_for(config_timeout_ms: Option<u64>) -> std::time::Duration {
    resolve_tool_timeout_bhs(
        config_timeout_ms,
        std::env::var("MCP_TOOL_TIMEOUT").ok().as_deref(),
    )
}

/// Pure core of [`mcp_tool_timeout_for`] (config + env injected for
/// testability); see that function for the `BHs` reference.
fn resolve_tool_timeout_bhs(
    config_timeout_ms: Option<u64>,
    env_value: Option<&str>,
) -> std::time::Duration {
    let n = config_timeout_ms
        .filter(|&ms| ms >= MCP_TOOL_TIMEOUT_MIN_MS)
        .or_else(|| {
            env_value
                .and_then(parse_int_base10_prefix)
                .filter(|&ms| ms > 0)
        })
        .unwrap_or(DEFAULT_CALL_TOOL_TIMEOUT_MS);
    std::time::Duration::from_millis(n.clamp(MCP_TOOL_TIMEOUT_MIN_MS, MCP_TOOL_TIMEOUT_MAX_MS))
}

/// Env-only shim retained for the existing test surface: equivalent to
/// `resolve_tool_timeout_bhs(None, env_value)`.
#[cfg(test)]
fn resolve_tool_timeout(env_value: Option<&str>) -> std::time::Duration {
    resolve_tool_timeout_bhs(None, env_value)
}

/// Default `tools/call` **idle** timeout for a stdio server — claude-code
/// `RMy = 1_800_000` (30 min). The idle timeout aborts a call after this many
/// ms of *silence* (no response and no `notifications/progress`), independent of
/// the overall [`mcp_tool_timeout_for`] ceiling.
const MCP_TOOL_IDLE_TIMEOUT_STDIO_MS: u64 = 1_800_000;
/// Default `tools/call` idle timeout for a *remote* (sse/http/ws) server —
/// claude-code `AMy = 300_000` (5 min).
const MCP_TOOL_IDLE_TIMEOUT_REMOTE_MS: u64 = 300_000;

/// Cadence at which the idle watchdog samples the silence deadline — claude-code
/// arms its silence check on a `setInterval(..., 30000)`, so the *effective*
/// idle-abort granularity is 30s (the first sample can only fire one interval
/// in). Mirrored here as the watchdog's poll interval.
const MCP_TOOL_IDLE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Idle/silence watchdog for a single `tools/call`. Samples `last_activity`
/// every [`MCP_TOOL_IDLE_CHECK_INTERVAL`]; once the silence since the last
/// inbound `notifications/progress` (or the call start) exceeds `idle_timeout`,
/// resolves to [`McpClientError::IdleTimeout`]. Only ever resolves to `Err`
/// (never `Ok`) — it is one arm of the `tools/call` race in
/// [`McpClient::call_tool_with_meta`]; when the call responds first this future
/// is dropped. Mirrors claude-code's silence check inside the 30s watchdog
/// `setInterval` (abort with the `"MCP tool idle timeout"` `Bn`).
async fn idle_watchdog(
    idle_timeout: std::time::Duration,
    last_activity: Arc<std::sync::Mutex<tokio::time::Instant>>,
    server: String,
    tool: String,
) -> Result<McpToolResultDto, McpClientError> {
    loop {
        tokio::time::sleep(MCP_TOOL_IDLE_CHECK_INTERVAL).await;
        let elapsed = last_activity
            .lock()
            .map(|a| a.elapsed())
            .unwrap_or_default();
        // `Date.now() - v > b` — strictly greater, matching the `GLd` watchdog.
        if elapsed > idle_timeout {
            return Err(McpClientError::IdleTimeout {
                server,
                tool,
                // `B = Math.floor((Date.now() - v) / 1000)`.
                secs: elapsed.as_secs(),
            });
        }
    }
}

/// Resolve the per-call **idle** timeout — a 1:1 port of claude-code `GLd`
/// (`services/mcp/client.ts`):
///
/// ```js
/// function GLd(e){
///   let t=e?.type??"stdio";
///   if(xMy.has(t))return 0;                       // xMy = {"sse-ide","ws-ide","sdk"}
///   let r=be.CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT ?? (t==="stdio"?RMy:AMy);
///   if(r<=0)return 0;                             // env 0 (or negative) disables
///   let n=e?.timeout!==void 0 && e.timeout>=1000 ? e.timeout : 0;
///   return Math.min(Math.max(r,n,1000), BHs(e))   // capped by the per-call ceiling
/// }
/// ```
///
/// Returns `0` when the idle timeout is disabled: for the in-process transports
/// (`SseIde`/`SdkControl`/`InProcess`, mirroring `xMy`), or when
/// `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` parses to `<= 0`. Otherwise the resolved
/// idle window is `max(env-or-default, config.timeout, 1000)` clamped **down** to
/// the [`mcp_tool_timeout_for`] (`BHs`) ceiling, so it never exceeds the hard
/// call timeout.
#[must_use]
pub fn mcp_tool_idle_timeout_for(
    config_timeout_ms: Option<u64>,
    transport_kind: McpTransportKind,
) -> std::time::Duration {
    resolve_idle_timeout_gld(
        config_timeout_ms,
        transport_kind,
        std::env::var("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT")
            .ok()
            .as_deref(),
        std::env::var("MCP_TOOL_TIMEOUT").ok().as_deref(),
    )
}

/// Pure core of [`mcp_tool_idle_timeout_for`] (config + env injected for
/// testability); see that function for the `GLd` reference.
fn resolve_idle_timeout_gld(
    config_timeout_ms: Option<u64>,
    transport_kind: McpTransportKind,
    idle_env: Option<&str>,
    tool_timeout_env: Option<&str>,
) -> std::time::Duration {
    // `xMy.has(type)` — the in-process transports carry no idle timeout.
    if transport_kind_is_in_process(transport_kind) {
        return std::time::Duration::ZERO;
    }
    // `be.CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT ?? (stdio ? RMy : AMy)`. The env
    // accessor parses a base-10 integer; a missing / non-numeric value falls to
    // the transport default. A value that parses `<= 0` disables the idle timeout.
    let r = match idle_env.and_then(parse_int_signed_base10_prefix) {
        Some(v) => v,
        None => i128::from(if matches!(transport_kind, McpTransportKind::Stdio) {
            MCP_TOOL_IDLE_TIMEOUT_STDIO_MS
        } else {
            MCP_TOOL_IDLE_TIMEOUT_REMOTE_MS
        }),
    };
    if r <= 0 {
        return std::time::Duration::ZERO;
    }
    // `r > 0` here, so narrowing to the ms domain is lossless (an absurdly large
    // env value saturates at the BHs ceiling anyway).
    let r = u64::try_from(r).unwrap_or(MCP_TOOL_TIMEOUT_MAX_MS);
    // `n = config.timeout>=1000 ? config.timeout : 0`.
    let n = config_timeout_ms
        .filter(|&ms| ms >= MCP_TOOL_TIMEOUT_MIN_MS)
        .unwrap_or(0);
    // `Math.min(Math.max(r, n, 1000), BHs(e))`. The BHs ceiling is always
    // `<= WLd` (i32::MAX ms), so the `u128 -> u64` narrowing never truncates.
    let ceiling =
        u64::try_from(resolve_tool_timeout_bhs(config_timeout_ms, tool_timeout_env).as_millis())
            .unwrap_or(MCP_TOOL_TIMEOUT_MAX_MS);
    let floor = r.max(n).max(MCP_TOOL_TIMEOUT_MIN_MS);
    std::time::Duration::from_millis(floor.min(ceiling))
}

/// The in-process MCP transports that carry no idle timeout — mirrors
/// claude-code's `xMy = new Set(["sse-ide","ws-ide","sdk"])` (`GLd`). These are
/// same-process bridges (IDE / SDK control), for which a silence watchdog is
/// meaningless.
///
/// All THREE `xMy` members are covered: `SseIde` = `"sse-ide"`, `WsIde` =
/// `"ws-ide"` (§10's variant — omitting it gave a `ws-ide` transport the
/// 5-minute `AMy` remote default the oracle disables), `SdkControl` = `"sdk"`.
/// `InProcess` has no oracle counterpart and is this port's own same-process
/// bridge, which belongs to the set for the same reason.
fn transport_kind_is_in_process(kind: McpTransportKind) -> bool {
    matches!(
        kind,
        McpTransportKind::SseIde
            | McpTransportKind::WsIde
            | McpTransportKind::SdkControl
            | McpTransportKind::InProcess
    )
}

/// Lower clamp / default for the HTTP non-GET **fetch** timeout — claude-code
/// `FLd = 60_000` (`jHs`). When no config `timeout` and no `MCP_TOOL_TIMEOUT`
/// env override resolve, the fetch timeout is exactly this floor.
const MCP_HTTP_FETCH_TIMEOUT_FLOOR_MS: u64 = 60_000;

/// Resolve the Streamable-HTTP **non-GET fetch** timeout — a 1:1 port of
/// claude-code `jHs` (`services/mcp/client.ts`):
///
/// ```js
/// function jHs(e){
///   let t=parseInt(process.env.MCP_TOOL_TIMEOUT||"",10),
///       n=(e?.timeout!==void 0 && e.timeout>=1000 ? e.timeout : void 0) ?? (t>0 ? t : void 0);
///   return n!==void 0 ? Math.min(Math.max(n,FLd),WLd) : FLd   // FLd=60000, WLd=2147483647
/// }
/// ```
///
/// This bounds the time-to-response-*headers* of an HTTP POST (the `fetch`
/// resolves once headers arrive; a streaming SSE body is read afterwards without
/// this bound — claude-code clears the timer in the `finally` of `await
/// fetch(...)`). Precedence: config `timeout` (`>= 1000ms`) → `MCP_TOOL_TIMEOUT`
/// env (`> 0`) → `60_000`. When either resolves, the value is clamped to
/// `[60_000, 2_147_483_647]`; otherwise it is exactly `60_000`.
#[must_use]
pub fn mcp_http_fetch_timeout_for(config_timeout_ms: Option<u64>) -> std::time::Duration {
    resolve_http_fetch_timeout_jhs(
        config_timeout_ms,
        std::env::var("MCP_TOOL_TIMEOUT").ok().as_deref(),
    )
}

/// Pure core of [`mcp_http_fetch_timeout_for`] (config + env injected for
/// testability); see that function for the `jHs` reference.
fn resolve_http_fetch_timeout_jhs(
    config_timeout_ms: Option<u64>,
    env_value: Option<&str>,
) -> std::time::Duration {
    let n = config_timeout_ms
        .filter(|&ms| ms >= MCP_TOOL_TIMEOUT_MIN_MS)
        .or_else(|| {
            env_value
                .and_then(parse_int_base10_prefix)
                .filter(|&ms| ms > 0)
        });
    let ms = match n {
        Some(v) => v.clamp(MCP_HTTP_FETCH_TIMEOUT_FLOOR_MS, MCP_TOOL_TIMEOUT_MAX_MS),
        None => MCP_HTTP_FETCH_TIMEOUT_FLOOR_MS,
    };
    std::time::Duration::from_millis(ms)
}

/// JS `parseInt(s, 10)` allowing a leading `-` (the idle-timeout env may be set
/// to a negative or `0` value to *disable* the watchdog, which
/// [`resolve_idle_timeout_gld`] treats via its `r <= 0` guard). Returns `None`
/// when no digits lead (JS `NaN` → the `??` default). Widened to `i128` so a
/// negative parses distinctly from absent.
fn parse_int_signed_base10_prefix(s: &str) -> Option<i128> {
    let t = s.trim_start();
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    let v = digits.parse::<i128>().ok()?;
    Some(if neg { -v } else { v })
}

/// JS `parseInt(s, 10)` for the non-negative case: skip leading ASCII
/// whitespace, accept an optional `+`, consume leading base-10 digits, and
/// ignore any trailing characters (`"100abc"` → `100`). Returns `None` when no
/// digits lead (JS `NaN`). A leading `-` also yields `None` — negative timeouts
/// are nonsensical and would be rejected by the `> 0` filter anyway (a safer,
/// documented divergence from JS, which would treat a negative as immediate).
fn parse_int_base10_prefix(s: &str) -> Option<u64> {
    let t = s.trim_start();
    let t = t.strip_prefix('+').unwrap_or(t);
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u64>().ok()
}

/// Wire-level shape of a `tools/list` response body.
#[derive(Debug, Deserialize)]
struct ToolsListResponse {
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
    #[serde(default)]
    tools: Vec<RawTool>,
}

/// Wire-level shape for one tool entry inside `tools/list`. The optional
/// `_meta` block carries claude-code-specific hints
/// (`anthropic/searchHint` for retrieval prefiltering, `anthropic/alwaysLoad`
/// to force-include the tool in the agent prompt even when the search hint
/// doesn't match, `anthropic/requiresUserInteraction` to mark a tool that
/// needs fresh interaction on every call).
#[derive(Debug, Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "inputSchema", default)]
    input_schema: serde_json::Value,
    #[serde(rename = "outputSchema", default)]
    output_schema: Option<serde_json::Value>,
    #[serde(default)]
    annotations: Option<lingxi_core::host::McpToolAnnotationsDto>,
    #[serde(default)]
    icons: Vec<lingxi_core::host::McpIconDto>,
    #[serde(default, rename = "_meta")]
    meta: Option<serde_json::Value>,
}

fn decode_tools_list_page(
    raw: serde_json::Value,
) -> Result<(Vec<RawTool>, Option<String>), McpClientError> {
    // `recursivelySanitizeUnicode(result.tools)` is applied to the response
    // before typed processing in the oracle.
    let response: ToolsListResponse = serde_json::from_value(recursively_sanitize_unicode(raw))
        .map_err(|error| McpClientError::Deserialize(error.to_string()))?;
    Ok((response.tools, response.next_cursor))
}

fn decode_prompts_list_page(
    raw: serde_json::Value,
) -> Result<(Vec<RawPrompt>, Option<String>), McpClientError> {
    let response: PromptsListResponse =
        serde_json::from_value(recursively_sanitize_unicode(raw))
            .map_err(|error| McpClientError::Deserialize(error.to_string()))?;
    Ok((response.prompts, response.next_cursor))
}

fn decode_resources_list_page(
    raw: serde_json::Value,
) -> Result<(Vec<RawResource>, Option<String>), McpClientError> {
    let response: ResourcesListResponse = serde_json::from_value(raw)
        .map_err(|error| McpClientError::Deserialize(error.to_string()))?;
    Ok((response.resources, response.next_cursor))
}

fn tool_meta_search_hint(meta: &Option<serde_json::Value>) -> Option<String> {
    meta.as_ref()
        .and_then(|meta| meta.get("anthropic/searchHint"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn tool_meta_always_load(meta: &Option<serde_json::Value>) -> Option<bool> {
    meta.as_ref()
        .and_then(|meta| meta.get("anthropic/alwaysLoad"))
        .and_then(serde_json::Value::as_bool)
}

fn tool_meta_requires_user_interaction(meta: &Option<serde_json::Value>) -> bool {
    meta.as_ref()
        .and_then(|meta| meta.get("anthropic/requiresUserInteraction"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Wire-level shape of a `tools/call` response body.
///
/// The MCP `CallToolResult` also carries two optional, arbitrary-JSON members:
/// `_meta` and `structuredContent`. Both are parsed as opaque
/// `Option<serde_json::Value>` and forwarded verbatim (no transformation),
/// matching claude-code (`services/mcp/client.ts` reads `result._meta` /
/// `result.structuredContent` straight off the raw result).
#[derive(Debug, Deserialize)]
struct ToolCallResponse {
    #[serde(default)]
    content: serde_json::Value,
    #[serde(rename = "isError", default)]
    is_error: bool,
    #[serde(rename = "_meta", default)]
    meta: Option<serde_json::Value>,
    #[serde(rename = "structuredContent", default)]
    structured_content: Option<serde_json::Value>,
}

/// Errors emitted by [`McpClient`] operations.
#[derive(Debug, Error)]
pub enum McpClientError {
    /// Tool call exceeded the configured timeout. Message format is wire-
    /// locked: `MCP server "<server>" tool "<tool>" timed out after <secs>s`.
    #[error("MCP server \"{server}\" tool \"{tool}\" timed out after {secs}s")]
    Timeout {
        /// Logical MCP server name from [`McpClient::new`].
        server: String,
        /// Tool name from the failing `tools/call` invocation.
        tool: String,
        /// Configured timeout (seconds).
        secs: u64,
    },
    /// Tool call produced no response and no `notifications/progress` for the
    /// resolved idle window ([`mcp_tool_idle_timeout_for`], `GLd`), so the
    /// silence watchdog aborted it. Message format is wire-locked to
    /// claude-code's idle-timeout `Bn(...)` string ("MCP tool idle timeout").
    #[error(
        "MCP server \"{server}\" tool \"{tool}\" sent no response or progress for {secs}s; \
aborting. If this server is configured in your MCP settings, set a per-server \"timeout\" \
(ms) to allow longer silent runs for just this server; otherwise set \
CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT (ms) globally (0 disables)."
    )]
    IdleTimeout {
        /// Logical MCP server name from [`McpClient::new`].
        server: String,
        /// Tool name from the stalled `tools/call` invocation.
        tool: String,
        /// Elapsed idle seconds at abort (`floor(idle_ms / 1000)`).
        secs: u64,
    },
    /// Underlying JSON-RPC transport returned an error response or framing
    /// failure; the inner string is the stringified `JsonRpcError`.
    #[error("JSON-RPC error: {0}")]
    Rpc(String),
    /// HTTP status metadata retained by streamable HTTP/SSE transports.
    #[error("HTTP {status}{detail}", detail = www_authenticate.as_ref().map(|value| format!(": {value}")).unwrap_or_default())]
    HttpResponse {
        /// HTTP response status.
        status: u16,
        /// `WWW-Authenticate` response header.
        www_authenticate: Option<String>,
        /// Original transport error text. Retained for the MCP SDK's narrow
        /// HTTP-400 stale-session phrases; never emitted as telemetry.
        message: String,
    },
    /// Native modern SDK or locally registered handler failure.
    #[error(transparent)]
    Result(#[from] McpResultError),
    /// Server returned a syntactically valid response that did not match
    /// the expected DTO shape.
    #[error("malformed response: {0}")]
    Deserialize(String),
    /// `initialize` handshake failed.
    #[error("initialize failed: {0}")]
    Initialize(String),
}

impl McpClientError {
    /// Whether this error permits one credential refresh and retry.
    #[must_use]
    pub fn is_auth_response(&self) -> bool {
        matches!(
            self,
            Self::HttpResponse {
                status: 401 | 403,
                ..
            }
        )
    }

    /// Whether this error matches the narrow HTTP MCP session-expired retry
    /// shape: a session-scoped endpoint returning 404, or a future-compatible
    /// remote `Connection closed` error after the transport tears itself down.
    #[must_use]
    pub fn is_session_expired(&self) -> bool {
        matches!(
            self,
            Self::HttpResponse {
                status: 404,
                message,
                ..
            } if !message.contains("Failed to open SSE stream")
        ) || matches!(
            self,
            Self::HttpResponse {
                status: 400,
                message,
                ..
            } if has_stale_session_phrase(message)
        ) || matches!(
            self,
            Self::Rpc(message)
                if (message.contains("code=-32000") && message.contains("Connection closed"))
                    || ((message.contains("code=400") || message.contains("HTTP 400"))
                        && has_stale_session_phrase(message))
        )
    }

    /// Low-cardinality code for the session-expired retry telemetry path.
    #[must_use]
    pub fn session_expired_error_code(&self) -> Option<&'static str> {
        if matches!(self, Self::HttpResponse { status: 404, .. }) {
            return Some("404");
        }
        if matches!(self, Self::HttpResponse { status: 400, .. }) {
            return Some("400");
        }
        None
    }
}

fn has_stale_session_phrase(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("server not initialized")
        || message.contains("no valid session id")
        || message.contains("mcp-session-id header is required")
}

fn mcp_client_error_from_result(error: McpResultError) -> McpClientError {
    match error {
        McpResultError::Connection(rpc) => mcp_client_error_from_rpc(&rpc),
        error => McpClientError::Result(error),
    }
}

fn mcp_client_error_from_rpc(error: &jsonrpc::ConnectionError) -> McpClientError {
    if let jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(remote)) = error {
        if let Some(data) = remote.data.as_ref() {
            if let Some(status) = data
                .get("httpStatus")
                .and_then(serde_json::Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
            {
                let www_authenticate = data
                    .get("wwwAuthenticate")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let body = data
                    .get("body")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty());
                let message = body.map_or_else(
                    || remote.message.clone(),
                    |body| format!("{}; {body}", remote.message),
                );
                return McpClientError::HttpResponse {
                    status,
                    www_authenticate,
                    message,
                };
            }
        }
    }

    let message = error.to_string();
    let Some(marker) = message.find("MCP_HTTP_STATUS=") else {
        return McpClientError::Rpc(message);
    };
    let metadata = &message[marker + "MCP_HTTP_STATUS=".len()..];
    let Some((status, rest)) = metadata.split_once(';') else {
        return McpClientError::Rpc(message);
    };
    let Ok(status) = status.parse::<u16>() else {
        return McpClientError::Rpc(message);
    };
    let www_authenticate = rest
        .strip_prefix("WWW_AUTHENTICATE=")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    McpClientError::HttpResponse {
        status,
        www_authenticate,
        message,
    }
}

/// `isEnvTruthy` (`envUtils.ts:32-37`): unset/empty ⇒ false; else `true` unless
/// the value is `"0"`, `"false"`, `"no"`, or `"off"` (case-insensitive, trimmed).
/// Reads the env var `name` from the process environment.
fn is_env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Err(_) => false,
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && !matches!(v.as_str(), "0" | "false" | "no" | "off")
        }
    }
}

#[cfg(test)]
mod constructor_tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use jsonrpc::{Connection, Mode};
    use tokio::sync::mpsc;

    /// Build a `Connection` over a fresh pair of `mpsc<Bytes>` channels and
    /// hand back both peer-side ends so the test can drive the dispatcher
    /// without standing up a full mock server.
    #[allow(clippy::type_complexity)]
    fn paired_connection() -> (Arc<Connection>, mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
        let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        let conn = Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        ));
        (conn, peer_to_us_tx, us_to_peer_rx)
    }

    #[test]
    fn stale_session_classifier_uses_http_400_body_and_omits_connection_code() {
        let error = jsonrpc::ConnectionError::Router(jsonrpc::RouterError::Remote(
            jsonrpc::messages::ResponseError {
                code: -32_001,
                message: "MCP_HTTP_STATUS=400;WWW_AUTHENTICATE=".to_string(),
                data: Some(serde_json::json!({
                    "httpStatus": 400,
                    "wwwAuthenticate": null,
                    "body": "Server not initialized"
                })),
            },
        ));
        let classified = mcp_client_error_from_rpc(&error);
        assert!(classified.is_session_expired());
        assert_eq!(classified.session_expired_error_code(), Some("400"));

        let connection_closed =
            McpClientError::Rpc("remote error: code=-32000, message=Connection closed".to_string());
        assert!(connection_closed.is_session_expired());
        assert_eq!(connection_closed.session_expired_error_code(), None);
    }

    #[test]
    fn failed_sse_open_is_not_a_stale_http_session() {
        let error = McpClientError::HttpResponse {
            status: 404,
            www_authenticate: None,
            message: "Failed to open SSE stream".to_string(),
        };
        assert!(!error.is_session_expired());
    }

    #[tokio::test]
    async fn constructor_compiles_and_stores_server_name() {
        let (conn, _peer_tx, _peer_rx) = paired_connection();
        let client =
            McpClient::new("filesystem", std::path::PathBuf::from("/tmp/work"), conn).await;
        assert_eq!(client.server_name(), "filesystem");
    }

    #[tokio::test]
    async fn notification_subscription_forwards_server_push_and_can_be_dropped() {
        let (conn, peer_tx, _peer_rx) = paired_connection();
        let client =
            McpClient::new("filesystem", std::path::PathBuf::from("/tmp/work"), conn).await;
        let mut notifications = client.subscribe_notifications();

        let mut frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/list_changed",
            "params": {}
        }))
        .unwrap();
        frame.push(b'\n');
        peer_tx.send(Bytes::from(frame)).await.unwrap();

        let notification =
            tokio::time::timeout(std::time::Duration::from_secs(2), notifications.next())
                .await
                .expect("notification subscription must receive a server push")
                .expect("the live connection must remain open");
        assert_eq!(notification.method, "notifications/resources/list_changed");
        assert_eq!(notification.params, serde_json::json!({}));

        // Dropping the stream releases only this receiver; the client remains
        // usable and other subscribers are not affected.
        drop(notifications);
    }

    #[tokio::test]
    async fn constructor_registers_roots_and_elicitation_handlers() {
        // We cannot peek inside the dispatcher map from outside the crate,
        // but we CAN exercise the registration path end-to-end: send a
        // `roots/list` request over the peer side and observe the response.
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let _client = McpClient::new(
            "filesystem",
            std::path::PathBuf::from("/Users/example/project"),
            conn,
        )
        .await;

        // Inject a `roots/list` request from the peer side.
        let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\"}\n";
        peer_tx
            .send(Bytes::from_static(req))
            .await
            .expect("send into broker");

        // Expect a response on the outbound channel.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("response within timeout")
            .expect("frame was sent");
        let text = std::str::from_utf8(&frame).expect("utf-8 frame");
        assert!(
            text.contains(r#""uri":"file:///Users/example/project""#),
            "roots/list handler not registered or response shape wrong: {text}",
        );

        // Inject an `elicitation/create` request as well.
        let req2 = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"elicitation/create\"}\n";
        peer_tx
            .send(Bytes::from_static(req2))
            .await
            .expect("send second request");
        let frame2 = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("response within timeout")
            .expect("frame was sent");
        let text2 = std::str::from_utf8(&frame2).expect("utf-8 frame");
        assert!(
            text2.contains(r#""action":"cancel""#),
            "elicitation/create handler not registered: {text2}",
        );
    }

    #[tokio::test]
    async fn modern_catalog_requests_use_required_meta_and_result_type() {
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let client = McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn)
            .await
            .with_negotiated_protocol(McpNegotiatedProtocol {
                era: McpProtocolEra::Modern,
                version: "2026-07-28".into(),
            });
        let handle = tokio::spawn(async move { client.list_tools().await });
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("tools/list request")
            .expect("request frame");
        let request: serde_json::Value = serde_json::from_slice(&frame).expect("request json");
        assert_eq!(request["method"], "tools/list");
        assert_eq!(request["params"]["_meta"].as_object().unwrap().len(), 3);
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {
                "resultType": "complete",
                "ttlMs": 0,
                "cacheScope": "private",
                "tools": []
            }
        });
        let mut bytes = serde_json::to_vec(&response).expect("response json");
        bytes.push(b'\n');
        peer_tx
            .send(Bytes::from(bytes))
            .await
            .expect("send response");
        assert!(handle
            .await
            .expect("list_tools join")
            .expect("modern list_tools")
            .is_empty());
    }

    #[tokio::test]
    async fn with_roots_advertises_additional_dirs_on_roots_list() {
        // A client built with additional roots answers `roots/list` with cwd
        // first, then each additional dir (parity 2.1.207 r1d()).
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let _client = McpClient::with_roots(
            "filesystem",
            std::path::PathBuf::from("/proj"),
            crate::new_shared_roots(vec![std::path::PathBuf::from("/tmp/extra")]),
            conn,
            None,
        )
        .await;

        let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\"}\n";
        peer_tx
            .send(Bytes::from_static(req))
            .await
            .expect("send into broker");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("response within timeout")
            .expect("frame was sent");
        let text = std::str::from_utf8(&frame).expect("utf-8 frame");
        assert!(
            text.contains(r#""roots":[{"uri":"file:///proj"},{"uri":"file:///tmp/extra"}]"#),
            "roots/list must advertise cwd + additional dir: {text}",
        );
    }

    #[tokio::test]
    async fn roots_list_reflects_a_runtime_add_without_reconnect() {
        // Parity 2.1.207 P1-08: a directory pushed into the SHARED roots cell
        // AFTER the client is built (a runtime `/add-dir`) shows up on the very
        // next `roots/list` the server issues — the handler reads the cell live,
        // so no reconnect / rebuild is needed.
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let roots = crate::new_shared_roots(Vec::new());
        let _client = McpClient::with_roots(
            "filesystem",
            std::path::PathBuf::from("/proj"),
            roots.clone(),
            conn,
            None,
        )
        .await;

        let issue = |peer_tx: tokio::sync::mpsc::Sender<Bytes>| async move {
            let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\"}\n";
            peer_tx
                .send(Bytes::from_static(req))
                .await
                .expect("send into broker");
        };

        // First roots/list: cwd-only.
        issue(peer_tx.clone()).await;
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("response within timeout")
            .expect("frame was sent");
        let text = std::str::from_utf8(&frame).expect("utf-8 frame");
        assert!(
            text.contains(r#""roots":[{"uri":"file:///proj"}]"#),
            "initial roots/list is cwd-only: {text}",
        );

        // Runtime add.
        roots
            .write()
            .unwrap()
            .push(std::path::PathBuf::from("/extra"));

        // Second roots/list now includes the live-added dir.
        issue(peer_tx.clone()).await;
        let frame2 = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("response within timeout")
            .expect("frame was sent");
        let text2 = std::str::from_utf8(&frame2).expect("utf-8 frame");
        assert!(
            text2.contains(r#""roots":[{"uri":"file:///proj"},{"uri":"file:///extra"}]"#),
            "roots/list must reflect the runtime add without reconnect: {text2}",
        );
    }

    #[tokio::test]
    async fn send_roots_list_changed_emits_notification_frame() {
        // The client can notify the server its roots changed — the wire frame
        // is a JSON-RPC notification (no id) for `notifications/roots/list_changed`.
        let (conn, _peer_tx, mut peer_rx) = paired_connection();
        let client = McpClient::new("filesystem", std::path::PathBuf::from("/proj"), conn).await;
        client.send_roots_list_changed();

        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("notification within timeout")
            .expect("frame was sent");
        let text = std::str::from_utf8(&frame).expect("utf-8 frame");
        assert!(
            text.contains(r#""method":"notifications/roots/list_changed""#),
            "must emit the roots/list_changed notification: {text}",
        );
        assert!(
            !text.contains(r#""id""#),
            "a notification carries no id: {text}",
        );
    }

    #[test]
    fn call_tool_passes_through_meta_and_structured_content() {
        // Drive the parse directly through the wire DTO: this is the unit that
        // owns `_meta` / `structuredContent` decoding. (A full transport
        // loopback is exercised by the constructor tests above; here we assert
        // the byte-faithful passthrough deterministically.)
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "ok" }],
            "isError": false,
            "_meta": { "anthropic/trace": "abc", "nested": { "k": [1, 2, 3] } },
            "structuredContent": { "rows": [{ "id": 7 }], "total": 1 },
        });
        let resp: super::ToolCallResponse =
            serde_json::from_value(body).expect("decode tools/call body");
        let dto = super::McpToolResultDto {
            content: resp.content,
            is_error: resp.is_error,
            meta: resp.meta,
            structured_content: resp.structured_content,
        };

        assert!(!dto.is_error);
        assert_eq!(
            dto.meta.as_ref().expect("meta present"),
            &serde_json::json!({ "anthropic/trace": "abc", "nested": { "k": [1, 2, 3] } }),
            "_meta must round-trip byte-for-byte",
        );
        assert_eq!(
            dto.structured_content.as_ref().expect("structured present"),
            &serde_json::json!({ "rows": [{ "id": 7 }], "total": 1 }),
            "structuredContent must round-trip byte-for-byte",
        );
    }

    #[test]
    fn call_tool_absent_meta_yields_none_no_empty_object() {
        // A result WITHOUT `_meta` / `structuredContent` must decode to `None`
        // for both — never an empty object, never a panic.
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "ok" }],
            "isError": false,
        });
        let resp: super::ToolCallResponse =
            serde_json::from_value(body).expect("decode tools/call body");
        assert!(resp.meta.is_none(), "absent _meta must be None");
        assert!(
            resp.structured_content.is_none(),
            "absent structuredContent must be None",
        );

        // Wholly empty body (server returned `{}`) is still safe.
        let empty: super::ToolCallResponse =
            serde_json::from_value(serde_json::json!({})).expect("decode empty body");
        assert!(empty.meta.is_none());
        assert!(empty.structured_content.is_none());
        assert!(!empty.is_error);
    }

    // -- MCP.3: request `_meta` carries `claudecode/toolUseId` ---------------

    #[tokio::test]
    async fn tools_call_request_carries_claudecode_tooluseid_meta() {
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let client =
            McpClient::new("filesystem", std::path::PathBuf::from("/tmp/work"), conn).await;

        // The call blocks until the peer responds — drive it on a task.
        let handle = tokio::spawn(async move {
            client
                .call_tool_with_progress(
                    "mcp__filesystem__read_file",
                    serde_json::json!({ "path": "/x" }),
                    Some("tu-123"),
                    None,
                )
                .await
        });

        // Inspect the outbound `tools/call` request.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("request within timeout")
            .expect("frame sent");
        let req: serde_json::Value = serde_json::from_slice(&frame).expect("json request");
        assert_eq!(req["method"], "tools/call");
        assert_eq!(req["params"]["name"], "read_file");
        // Byte-exact key (claude-code `client.ts:1842`).
        assert_eq!(
            req["params"]["_meta"]["claudecode/toolUseId"], "tu-123",
            "request _meta must carry the byte-exact claudecode/toolUseId key: {req}",
        );
        // No progress callback was wired → no progressToken minted.
        assert!(
            req["params"]["_meta"].get("progressToken").is_none(),
            "progressToken must be absent when no on_progress is supplied: {req}",
        );

        // Respond so the awaiting call resolves.
        let id = req["id"].clone();
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": "ok" }], "isError": false },
        });
        let mut bytes = serde_json::to_vec(&resp).expect("encode response");
        bytes.push(b'\n');
        peer_tx
            .send(Bytes::from(bytes))
            .await
            .expect("send response");

        let dto = handle.await.expect("join").expect("call ok");
        assert!(!dto.is_error);
    }

    #[tokio::test]
    async fn tools_call_request_omits_meta_when_no_tool_use_id() {
        // The plain `call_tool` path threads no toolUseId → the request must
        // carry no `_meta` block at all (no empty-object placeholder).
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let client =
            McpClient::new("filesystem", std::path::PathBuf::from("/tmp/work"), conn).await;

        let handle = tokio::spawn(async move {
            client
                .call_tool("mcp__filesystem__read_file", serde_json::json!({}))
                .await
        });

        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("request within timeout")
            .expect("frame sent");
        let req: serde_json::Value = serde_json::from_slice(&frame).expect("json request");
        assert_eq!(req["params"]["name"], "read_file");
        assert!(
            req["params"].get("_meta").is_none(),
            "_meta must be omitted when no toolUseId is threaded: {req}",
        );

        let id = req["id"].clone();
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [], "isError": false },
        });
        let mut bytes = serde_json::to_vec(&resp).expect("encode response");
        bytes.push(b'\n');
        peer_tx
            .send(Bytes::from(bytes))
            .await
            .expect("send response");
        let _ = handle.await.expect("join").expect("call ok");
    }

    // -- MCP.4: `notifications/progress` is forwarded to the callback --------

    #[tokio::test]
    async fn progress_notification_is_forwarded_to_callback() {
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let client =
            McpClient::new("filesystem", std::path::PathBuf::from("/tmp/work"), conn).await;

        let seen: Arc<std::sync::Mutex<Vec<McpProgressEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let cb: McpProgressCallback = Arc::new(move |ev: McpProgressEvent| {
            seen_cb.lock().expect("lock").push(ev);
        });

        let handle = tokio::spawn(async move {
            client
                .call_tool_with_progress(
                    "mcp__filesystem__read_file",
                    serde_json::json!({}),
                    Some("tu-9"),
                    Some(cb),
                )
                .await
        });

        // Read the request and recover the minted progressToken.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("request within timeout")
            .expect("frame sent");
        let req: serde_json::Value = serde_json::from_slice(&frame).expect("json request");
        let token = req["params"]["_meta"]["progressToken"].clone();
        assert!(
            token.is_string(),
            "progressToken must be minted into _meta when on_progress is wired: {req}",
        );

        // Server addresses a progress notification to that token.
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": { "progressToken": token, "progress": 3, "total": 10, "message": "halfway" },
        });
        let mut nbytes = serde_json::to_vec(&notif).expect("encode notif");
        nbytes.push(b'\n');
        peer_tx.send(Bytes::from(nbytes)).await.expect("send notif");

        // Poll until the forwarder delivers the event.
        let mut delivered = false;
        for _ in 0..100 {
            if !seen.lock().expect("lock").is_empty() {
                delivered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            delivered,
            "progress callback must fire for a token-matching notification",
        );

        // Respond so the call resolves.
        let id = req["id"].clone();
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [], "isError": false },
        });
        let mut bytes = serde_json::to_vec(&resp).expect("encode response");
        bytes.push(b'\n');
        peer_tx
            .send(Bytes::from(bytes))
            .await
            .expect("send response");
        let _ = handle.await.expect("join").expect("call ok");

        let events = seen.lock().expect("lock");
        assert_eq!(events.len(), 1, "exactly one progress event forwarded");
        assert_eq!(
            events[0],
            McpProgressEvent {
                progress: 3.0,
                total: Some(10.0),
                message: Some("halfway".to_string()),
            },
        );
    }

    #[tokio::test]
    async fn progress_notification_with_mismatched_token_is_ignored() {
        // A notification carrying a different progressToken must NOT fire the
        // callback (the forwarder matches strictly by token).
        let (conn, peer_tx, mut peer_rx) = paired_connection();
        let client =
            McpClient::new("filesystem", std::path::PathBuf::from("/tmp/work"), conn).await;

        let seen: Arc<std::sync::Mutex<Vec<McpProgressEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let cb: McpProgressCallback = Arc::new(move |ev: McpProgressEvent| {
            seen_cb.lock().expect("lock").push(ev);
        });

        let handle = tokio::spawn(async move {
            client
                .call_tool_with_progress(
                    "mcp__filesystem__read_file",
                    serde_json::json!({}),
                    Some("tu-1"),
                    Some(cb),
                )
                .await
        });

        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("request within timeout")
            .expect("frame sent");
        let req: serde_json::Value = serde_json::from_slice(&frame).expect("json request");

        // Wrong token.
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": { "progressToken": "some-other-token", "progress": 1 },
        });
        let mut nbytes = serde_json::to_vec(&notif).expect("encode notif");
        nbytes.push(b'\n');
        peer_tx.send(Bytes::from(nbytes)).await.expect("send notif");

        // Give the forwarder a chance to (incorrectly) deliver.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let id = req["id"].clone();
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [], "isError": false },
        });
        let mut bytes = serde_json::to_vec(&resp).expect("encode response");
        bytes.push(b'\n');
        peer_tx
            .send(Bytes::from(bytes))
            .await
            .expect("send response");
        let _ = handle.await.expect("join").expect("call ok");

        assert!(
            seen.lock().expect("lock").is_empty(),
            "callback must not fire for a non-matching progressToken",
        );
    }

    // ── Server-level `alwaysLoad` config (parity 2.1.207 P2-01) ─────────────

    /// Drive `tools/list` and return the client's decoded tools after
    /// responding with `tools_json`. Shared by the alwaysLoad tests below.
    async fn list_tools_with(
        client: McpClient,
        peer_tx: mpsc::Sender<Bytes>,
        mut peer_rx: mpsc::Receiver<Bytes>,
        tools_json: serde_json::Value,
    ) -> Vec<McpToolDto> {
        let handle = tokio::spawn(async move { client.list_tools().await });
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("tools/list request within timeout")
            .expect("frame sent");
        let req: serde_json::Value = serde_json::from_slice(&frame).expect("json request");
        assert_eq!(req["method"], "tools/list");
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req["id"].clone(),
            "result": { "tools": tools_json },
        });
        let mut bytes = serde_json::to_vec(&resp).expect("encode response");
        bytes.push(b'\n');
        peer_tx
            .send(Bytes::from(bytes))
            .await
            .expect("send response");
        handle.await.expect("join").expect("list_tools ok")
    }

    #[tokio::test]
    async fn server_always_load_forces_every_tool_always_loaded() {
        let (conn, peer_tx, peer_rx) = paired_connection();
        // Server-level alwaysLoad=true → ALL tools marked Some(true), even the
        // one that carries no per-tool `_meta` at all.
        let client = McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn)
            .await
            .with_config_options(None, true, Vec::new(), BTreeMap::new());
        let tools = list_tools_with(
            client,
            peer_tx,
            peer_rx,
            serde_json::json!([
                { "name": "a", "description": "A", "inputSchema": {} },
                { "name": "b", "description": "B", "inputSchema": {},
                  "_meta": { "anthropic/searchHint": "shell" } },
            ]),
        )
        .await;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].always_load, Some(true));
        assert_eq!(tools[1].always_load, Some(true));
    }

    #[tokio::test]
    async fn without_server_always_load_per_tool_bit_is_preserved() {
        let (conn, peer_tx, peer_rx) = paired_connection();
        // config_always_load defaults to false → the per-tool `_meta` bit wins:
        // tool `a` (no meta) stays None; tool `b` (alwaysLoad=true) stays true.
        let client = McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn).await;
        let tools = list_tools_with(
            client,
            peer_tx,
            peer_rx,
            serde_json::json!([
                { "name": "a", "description": "A", "inputSchema": {} },
                { "name": "b", "description": "B", "inputSchema": {},
                  "_meta": { "anthropic/alwaysLoad": true } },
            ]),
        )
        .await;
        assert_eq!(tools[0].always_load, None);
        assert_eq!(tools[1].always_load, Some(true));
    }

    // ── `requiresUserInteraction` (§27b) ─────────────────────────────────────

    #[tokio::test]
    async fn requires_user_interaction_meta_is_forwarded_onto_the_dto() {
        let (conn, peer_tx, peer_rx) = paired_connection();
        let client = McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn).await;
        let tools = list_tools_with(
            client,
            peer_tx,
            peer_rx,
            serde_json::json!([
                { "name": "a", "description": "A", "inputSchema": {} },
                { "name": "b", "description": "B", "inputSchema": {},
                  "outputSchema": { "type": "object", "properties": { "ok": { "type": "boolean" } } },
                  "annotations": { "title": "B", "readOnlyHint": true,
                    "vendor/riskTier": "reviewed" },
                  "icons": [{ "src": "https://example.invalid/icon.svg",
                    "vendor/accent": "blue" }],
                  "_meta": {
                    "anthropic/requiresUserInteraction": true,
                    "openai/outputTemplate": "ui://example/widget.html"
                  } },
            ]),
        )
        .await;
        assert_eq!(tools.len(), 2);
        // No `_meta` at all ⇒ defaults to false (non-claude-code servers
        // decode cleanly).
        assert!(!tools[0].requires_user_interaction);
        // `_meta.anthropic/requiresUserInteraction: true` ⇒ forwarded as-is.
        assert!(tools[1].requires_user_interaction);
        assert_eq!(
            tools[1].output_schema,
            Some(serde_json::json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } }
            }))
        );
        assert_eq!(
            tools[1]
                .annotations
                .as_ref()
                .and_then(|a| a.title.as_deref()),
            Some("B")
        );
        assert_eq!(tools[1].icons.len(), 1);
        assert_eq!(
            tools[1]
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.extra.get("vendor/riskTier")),
            Some(&serde_json::json!("reviewed"))
        );
        assert_eq!(
            tools[1].icons[0].extra.get("vendor/accent"),
            Some(&serde_json::json!("blue"))
        );
        assert_eq!(
            tools[1].meta,
            Some(serde_json::json!({
                "anthropic/requiresUserInteraction": true,
                "openai/outputTemplate": "ui://example/widget.html"
            }))
        );
    }

    #[tokio::test]
    async fn config_permission_ceilings_collapse_to_strictest_side_map() {
        let (conn, _peer_tx, _peer_rx) = paired_connection();
        let client = McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn)
            .await
            .with_config_options(
                None,
                false,
                vec![
                    McpConfiguredToolPolicyDto {
                        name: "write".into(),
                        permission_policy: None,
                        org_max_permission: Some(McpPermissionCeiling::Ask),
                    },
                    McpConfiguredToolPolicyDto {
                        name: "write".into(),
                        permission_policy: None,
                        org_max_permission: Some(McpPermissionCeiling::Deny),
                    },
                ],
                BTreeMap::from([(String::from("write"), McpPermissionCeiling::Ask)]),
            );
        let mut tools = vec![McpToolDto {
            server_name: "srv".into(),
            tool_name: "write".into(),
            description: "write".into(),
            input_schema: serde_json::json!({}),
            output_schema: None,
            annotations: None,
            icons: Vec::new(),
            meta: None,
            full_name: "mcp__srv__write".into(),
            search_hint: None,
            always_load: None,
            requires_user_interaction: false,
        }];
        client.apply_config_permission_ceilings(&mut tools);
        assert_eq!(
            client.effective_max_permission("write"),
            Some(McpPermissionCeiling::Deny)
        );
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::{
        parse_int_base10_prefix, parse_int_signed_base10_prefix, resolve_http_fetch_timeout_jhs,
        resolve_idle_timeout_gld, resolve_tool_timeout, resolve_tool_timeout_bhs,
        DEFAULT_CALL_TOOL_TIMEOUT,
    };
    use lingxi_core::host::McpTransportKind;
    use std::time::Duration;

    #[test]
    fn default_value_is_byte_locked_to_claude_code() {
        // DEFAULT_MCP_TOOL_TIMEOUT_MS = 100_000_000 (client.ts:211).
        assert_eq!(
            DEFAULT_CALL_TOOL_TIMEOUT,
            Duration::from_millis(100_000_000)
        );
    }

    #[test]
    fn parse_int_mirrors_js_parse_int() {
        assert_eq!(parse_int_base10_prefix("5000"), Some(5000));
        assert_eq!(parse_int_base10_prefix("  42  "), Some(42)); // leading ws skipped
        assert_eq!(parse_int_base10_prefix("+7"), Some(7)); // optional plus
        assert_eq!(parse_int_base10_prefix("100abc"), Some(100)); // trailing garbage ignored
        assert_eq!(parse_int_base10_prefix("0"), Some(0));
        assert_eq!(parse_int_base10_prefix(""), None); // NaN
        assert_eq!(parse_int_base10_prefix("abc"), None); // NaN
        assert_eq!(parse_int_base10_prefix("-5"), None); // safer-than-JS: rejected
    }

    #[test]
    fn resolve_uses_env_else_default() {
        // unset / unparseable / zero → default (JS `|| default`, 0/NaN falsy).
        assert_eq!(resolve_tool_timeout(None), DEFAULT_CALL_TOOL_TIMEOUT);
        assert_eq!(resolve_tool_timeout(Some("")), DEFAULT_CALL_TOOL_TIMEOUT);
        assert_eq!(resolve_tool_timeout(Some("abc")), DEFAULT_CALL_TOOL_TIMEOUT);
        assert_eq!(resolve_tool_timeout(Some("0")), DEFAULT_CALL_TOOL_TIMEOUT);
        // a positive integer (ms) above the 1000ms floor is honored verbatim.
        assert_eq!(
            resolve_tool_timeout(Some("30000")),
            Duration::from_millis(30_000)
        );
        // BHs clamps the FINAL value to >= 1000ms (`Math.max(n, 1000)`): an env
        // value below the floor is raised to 1000 (parity 2.1.207 P2-01 —
        // previously this returned 250ms un-clamped).
        assert_eq!(
            resolve_tool_timeout(Some("250abc")),
            Duration::from_millis(1000)
        );
    }

    // ── BHs config-timeout precedence + clamp (parity 2.1.207 P2-01) ────────

    #[test]
    fn config_timeout_wins_over_env_when_at_least_1000() {
        // config.timeout (>=1000) beats env MCP_TOOL_TIMEOUT.
        assert_eq!(
            resolve_tool_timeout_bhs(Some(45_000), Some("9000")),
            Duration::from_millis(45_000)
        );
    }

    #[test]
    fn config_timeout_below_1000_is_ignored_falls_through_to_env() {
        // BHs treats a config.timeout < 1000 as `void 0` → env is consulted next.
        assert_eq!(
            resolve_tool_timeout_bhs(Some(500), Some("9000")),
            Duration::from_millis(9000)
        );
        // ...and with no usable env, the default.
        assert_eq!(
            resolve_tool_timeout_bhs(Some(999), None),
            DEFAULT_CALL_TOOL_TIMEOUT
        );
    }

    #[test]
    fn config_timeout_none_matches_env_only_path() {
        assert_eq!(
            resolve_tool_timeout_bhs(None, Some("30000")),
            resolve_tool_timeout(Some("30000"))
        );
    }

    #[test]
    fn resolved_timeout_is_clamped_to_i32_max() {
        // WLd = 2_147_483_647 (i32::MAX ms) is the upper clamp.
        assert_eq!(
            resolve_tool_timeout_bhs(Some(9_999_999_999), None),
            Duration::from_millis(2_147_483_647)
        );
        // The 1e8 default is within range and passes through unclamped.
        assert_eq!(
            resolve_tool_timeout_bhs(None, None),
            Duration::from_millis(100_000_000)
        );
    }

    // ── GLd idle-timeout resolver (parity 2.1.207 P2-01 remainder) ──────────

    #[test]
    fn idle_default_is_30min_for_stdio_5min_for_remote() {
        // No env, no config timeout → the transport default (RMy / AMy), which is
        // below the 1e8 BHs ceiling so it passes through.
        assert_eq!(
            resolve_idle_timeout_gld(None, McpTransportKind::Stdio, None, None),
            Duration::from_millis(1_800_000)
        );
        for kind in [
            McpTransportKind::Sse,
            McpTransportKind::Http,
            McpTransportKind::WebSocket,
        ] {
            assert_eq!(
                resolve_idle_timeout_gld(None, kind, None, None),
                Duration::from_millis(300_000),
                "remote transport {kind:?} idle default must be 5 min",
            );
        }
    }

    #[test]
    fn idle_is_zero_for_in_process_transports() {
        // xMy = {"sse-ide","ws-ide","sdk"} → no idle timeout. lingxi's in-process
        // kinds (SseIde / WsIde / SdkControl / InProcess) mirror that set —
        // `WsIde` is §10's new variant and is the literal `"ws-ide"` member of
        // `xMy` (oracle @182263592 / @182470659:
        // `var er=new Set(["sse-ide","ws-ide","sdk"]); function tr(e){let
        // t=e?.type??"stdio"; if(er.has(t))return 0; …}`), so it must NOT fall
        // through to the 5-minute remote default.
        for kind in [
            McpTransportKind::SseIde,
            McpTransportKind::WsIde,
            McpTransportKind::SdkControl,
            McpTransportKind::InProcess,
        ] {
            assert_eq!(
                resolve_idle_timeout_gld(Some(600_000), kind, Some("120000"), None),
                Duration::ZERO,
                "in-process transport {kind:?} must have NO idle timeout",
            );
        }
    }

    #[test]
    fn idle_env_overrides_default_and_zero_disables() {
        // Env `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` beats the transport default.
        assert_eq!(
            resolve_idle_timeout_gld(None, McpTransportKind::Stdio, Some("120000"), None),
            Duration::from_millis(120_000)
        );
        // Env <= 0 disables the watchdog entirely (`if(r<=0)return 0`).
        assert_eq!(
            resolve_idle_timeout_gld(None, McpTransportKind::Stdio, Some("0"), None),
            Duration::ZERO
        );
        assert_eq!(
            resolve_idle_timeout_gld(None, McpTransportKind::Http, Some("-1"), None),
            Duration::ZERO
        );
    }

    #[test]
    fn idle_floor_is_max_of_env_config_and_1000_capped_by_bhs() {
        // `Math.min(Math.max(r, n, 1000), BHs(e))`. config.timeout raises the
        // floor when it exceeds the env/default (n = config when >= 1000).
        // Remote default r=300_000, config=600_000 → floor 600_000 (< 1e8 ceiling).
        assert_eq!(
            resolve_idle_timeout_gld(Some(600_000), McpTransportKind::Http, None, None),
            Duration::from_millis(600_000)
        );
        // The idle window is capped DOWN by the BHs ceiling. With config.timeout
        // = 5000, BHs = 5000, and the stdio default r = 1_800_000, so the idle
        // window collapses to the 5000ms ceiling.
        assert_eq!(
            resolve_idle_timeout_gld(Some(5_000), McpTransportKind::Stdio, None, None),
            Duration::from_millis(5_000)
        );
    }

    #[test]
    fn idle_signed_parse_distinguishes_absent_from_nonpositive() {
        assert_eq!(parse_int_signed_base10_prefix(""), None); // absent → default applies
        assert_eq!(parse_int_signed_base10_prefix("abc"), None);
        assert_eq!(parse_int_signed_base10_prefix("0"), Some(0));
        assert_eq!(parse_int_signed_base10_prefix("-1"), Some(-1));
        assert_eq!(parse_int_signed_base10_prefix("  90000x"), Some(90_000));
        assert_eq!(parse_int_signed_base10_prefix("+42"), Some(42));
    }

    // ── jHs HTTP non-GET fetch timeout (parity 2.1.207 P2-01 remainder) ─────

    #[test]
    fn fetch_timeout_defaults_to_60s_floor() {
        // No config, no env → exactly FLd = 60_000 (NOT the 1e8 BHs default).
        assert_eq!(
            resolve_http_fetch_timeout_jhs(None, None),
            Duration::from_millis(60_000)
        );
        // Sub-floor config/env is clamped UP to 60_000.
        assert_eq!(
            resolve_http_fetch_timeout_jhs(Some(5_000), None),
            Duration::from_millis(60_000)
        );
        assert_eq!(
            resolve_http_fetch_timeout_jhs(None, Some("30000")),
            Duration::from_millis(60_000)
        );
    }

    #[test]
    fn fetch_timeout_config_wins_over_env_and_clamps_to_i32_max() {
        // config.timeout (>= 1000) beats env, above the 60s floor → verbatim.
        assert_eq!(
            resolve_http_fetch_timeout_jhs(Some(90_000), Some("120000")),
            Duration::from_millis(90_000)
        );
        // config < 1000 is ignored → env consulted (env 120000 → 120000).
        assert_eq!(
            resolve_http_fetch_timeout_jhs(Some(500), Some("120000")),
            Duration::from_millis(120_000)
        );
        // Upper clamp WLd = 2_147_483_647.
        assert_eq!(
            resolve_http_fetch_timeout_jhs(Some(9_999_999_999), None),
            Duration::from_millis(2_147_483_647)
        );
    }

    // ── idle watchdog behavior (parity 2.1.207 P2-01 remainder) ─────────────

    #[tokio::test(start_paused = true)]
    async fn idle_watchdog_aborts_after_silence_window() {
        use super::{idle_watchdog, McpClientError};
        use std::sync::{Arc, Mutex};

        let last = Arc::new(Mutex::new(tokio::time::Instant::now()));
        let wd = tokio::spawn(idle_watchdog(
            Duration::from_millis(1_000),
            last.clone(),
            "srv".to_string(),
            "read".to_string(),
        ));
        // Let the watchdog reach its first 30s sample, then advance one interval:
        // elapsed (~31s) exceeds the 1s idle window → abort.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(31)).await;

        let err = wd.await.expect("watchdog join").expect_err("must abort");
        match err {
            McpClientError::IdleTimeout { server, tool, secs } => {
                assert_eq!(server, "srv");
                assert_eq!(tool, "read");
                assert!(secs >= 30, "elapsed idle secs floor was {secs}");
            }
            other => panic!("expected IdleTimeout, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_watchdog_holds_while_activity_is_fresh() {
        use super::idle_watchdog;
        use std::sync::{Arc, Mutex};

        // A 90s idle window (three 30s ticks). One tick of silence (31s) is well
        // under the window, so the watchdog must NOT have fired yet.
        let last = Arc::new(Mutex::new(tokio::time::Instant::now()));
        let wd = tokio::spawn(idle_watchdog(
            Duration::from_secs(90),
            last.clone(),
            "srv".to_string(),
            "read".to_string(),
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        assert!(
            !wd.is_finished(),
            "watchdog must not abort inside its window"
        );

        // Liveness (a progress note) resets the timer; another sub-window tick
        // still must not fire.
        *last.lock().unwrap() = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        assert!(
            !wd.is_finished(),
            "a reset within the window must keep the call alive"
        );
        wd.abort();
    }
}

#[cfg(test)]
mod sanitization_tests {
    use super::{partially_sanitize_unicode, recursively_sanitize_unicode};

    /// Verify that ASCII Smuggling Unicode Tag characters (U+E0000 block) are
    /// stripped — the primary HackerOne #3086545 attack vector.
    #[test]
    fn strips_unicode_tag_characters() {
        // U+E0048 is "TAG LATIN CAPITAL LETTER H" (invisible to users, visible
        // to the model). Injecting these encodes a hidden prompt.
        let malicious = "\u{E0048}\u{E0065}\u{E006C}\u{E006C}\u{E006F}";
        let clean = partially_sanitize_unicode(malicious);
        assert!(
            clean.is_empty(),
            "tag chars must be fully stripped, got: {clean:?}"
        );
    }

    #[test]
    fn strips_zero_width_chars() {
        // U+200B ZERO WIDTH SPACE — invisible but MCP-injectable.
        let input = "hello\u{200B}world";
        let clean = partially_sanitize_unicode(input);
        assert_eq!(clean, "helloworld");
    }

    #[test]
    fn strips_bom() {
        let input = "\u{FEFF}text";
        let clean = partially_sanitize_unicode(input);
        assert_eq!(clean, "text");
    }

    #[test]
    fn strips_private_use_area() {
        // U+E000 is the first BMP private-use code point.
        let input = "a\u{E000}b";
        let clean = partially_sanitize_unicode(input);
        assert_eq!(clean, "ab");
    }

    #[test]
    fn preserves_normal_text_unchanged() {
        let inputs = ["hello world", "こんにちは", "café", "123", "αβγ"];
        for input in &inputs {
            let clean = partially_sanitize_unicode(input);
            assert_eq!(&clean, input, "normal text must not be mutated");
        }
    }

    #[test]
    fn recursive_sanitizes_nested_json() {
        let value = serde_json::json!({
            "name": "tool\u{200B}name",
            "description": "desc\u{FEFF}end",
            "schema": {
                "key\u{E000}": "val\u{E0048}"
            },
            "items": ["\u{200B}a", "\u{200F}b"]
        });
        let clean = recursively_sanitize_unicode(value);
        assert_eq!(clean["name"], "toolname");
        assert_eq!(clean["description"], "descend");
        assert_eq!(clean["schema"]["key"], "val");
        let arr = clean["items"].as_array().unwrap();
        assert_eq!(arr[0], "a");
        assert_eq!(arr[1], "b");
    }

    #[test]
    fn recursive_preserves_non_string_primitives() {
        let value = serde_json::json!({
            "count": 42,
            "flag": true,
            "empty": null
        });
        let clean = recursively_sanitize_unicode(value.clone());
        assert_eq!(clean["count"], 42);
        assert_eq!(clean["flag"], true);
        assert!(clean["empty"].is_null());
    }
}
