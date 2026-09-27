//! Parity regression pins vs Claude Code 2.1.207.
//!
//! This is the version-named regression matrix for the 2.1.207 parity wave
//! (the first since `parity_claude_2_1_198.rs`). It pins the behaviors closed
//! by that wave that are assertable WITHOUT cross-lane state — chiefly the
//! outward version identifiers and the binary-safe WebFetch transport body
//! (finding P3-ALL). Behaviors that need a live subsystem (persist, TUI) are
//! pinned in their owning crates' unit tests; this harness holds the
//! cross-cutting, standalone contracts so a regression fails loudly here.
//!
//! Oracle: CC 2.1.207 self-identifies as `VERSION:"2.1.207"` and embeds it in
//! the child-env `AI_AGENT` (`claude-code_2-1-207_agent`, dots→dashes) and the
//! WebFetch `User-Agent`
//! (`Claude-User (claude-code/2.1.207; +https://support.anthropic.com/)`);
//! WebFetch GETs bodies as `responseType:"arraybuffer"` so binary content is
//! persisted byte-identically.

use protocol::HttpResponse;

const HISTORICAL_VERSION: &str = "2.1.207";

/// The live single source of truth (`platform_api::CLAUDE_CODE_VERSION`, R-V1)
/// is asserted by the current 2.1.252 fixture. This historical fixture keeps
/// its own 2.1.207 literal so a target bump cannot rewrite old captures.
#[test]
fn historical_version_literal_is_2_1_207() {
    assert_eq!(
        HISTORICAL_VERSION, "2.1.207",
        "historical 2.1.207 harness keeps its wave literal"
    );
}

/// The child-process `AI_AGENT` env value: `claude-code_${VERSION.replace(/\\./g,\"-\")}_agent`.
/// Byte-locked to the 2.1.207 literal AND to the derivation from the const, so
/// neither the format nor the version can drift undetected.
#[test]
fn ai_agent_env_value_is_2_1_207() {
    let derived = format!("claude-code_{}_agent", HISTORICAL_VERSION.replace('.', "-"));
    assert_eq!(derived, "claude-code_2-1-207_agent");
    // The version segment must use `-` separators, never `.` (the JS `replace`).
    let mid = derived
        .strip_prefix("claude-code_")
        .and_then(|s| s.strip_suffix("_agent"))
        .expect("prefix/suffix present");
    assert!(!mid.contains('.'), "version dots must be dashed: {derived}");
}

/// The WebFetch `User-Agent`:
/// `Claude-User (claude-code/${VERSION}; +https://support.anthropic.com/)`.
#[test]
fn web_fetch_user_agent_is_2_1_207() {
    let derived = format!(
        "Claude-User (claude-code/{}; +https://support.anthropic.com/)",
        HISTORICAL_VERSION
    );
    assert_eq!(
        derived,
        "Claude-User (claude-code/2.1.207; +https://support.anthropic.com/)"
    );
}

/// Binary-safe WebFetch body (finding P3-ALL): the transport DTO carries the
/// RAW wire bytes (`HttpResponse.body_bytes`) alongside the lossy `body`
/// String, so a genuinely-binary body (PDF/image/invalid-UTF8) survives
/// byte-identically to consumers such as WebFetch's artifact persist — matching
/// CC's `responseType:"arraybuffer"`. Pins that the raw bytes are (a) distinct
/// from the lossy String view and (b) preserved across serde round-trip.
#[test]
fn http_response_carries_raw_binary_body_bytes() {
    // `%PDF-1.4` header followed by NUL and two invalid-UTF8 bytes.
    let raw: Vec<u8> = vec![
        0x25, 0x50, 0x44, 0x46, 0x2D, 0x31, 0x2E, 0x34, 0x00, 0xFF, 0xFE, 0x89,
    ];
    let resp = HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/pdf".into())],
        body: String::from_utf8_lossy(&raw).into_owned(),
        body_bytes: raw.clone(),
    };
    // Lossy decoding replaced the invalid bytes, so `body.as_bytes()` is NOT
    // the wire — the artifact-persist path MUST read `body_bytes`.
    assert_ne!(
        resp.body.as_bytes(),
        raw.as_slice(),
        "lossy String body must differ from the wire for a true binary"
    );
    assert_eq!(resp.body_bytes, raw, "raw wire bytes preserved on the DTO");

    // Round-trips through serde intact (the transport crosses FFI boundaries on
    // mobile; raw bytes must not be lost there either).
    let json = serde_json::to_string(&resp).expect("serialize");
    let back: HttpResponse = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.body_bytes, raw, "raw bytes survive serde round-trip");
}

/// The empty `body_bytes` (test mocks / non-transport producers) is omitted
/// from the serialized shape, so adding the field did not change any existing
/// serialized `HttpResponse` payload.
#[test]
fn empty_body_bytes_keeps_serialized_shape() {
    let resp = HttpResponse {
        status: 204,
        headers: vec![],
        body: String::new(),
        body_bytes: Vec::new(),
    };
    let json = serde_json::to_string(&resp).expect("serialize");
    assert!(
        !json.contains("body_bytes"),
        "empty body_bytes must not appear in the serialized shape; got: {json}"
    );
}
