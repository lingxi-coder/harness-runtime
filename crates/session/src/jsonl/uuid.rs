//! UUID validation — 1:1 port of `claude-code/src/utils/sessionStoragePortable.ts:23-29`.

use once_cell::sync::Lazy;
use regex::Regex;

static UUID_RE: Lazy<Regex> = Lazy::new(|| {
    // case-insensitive; `^...$` anchored; v4-compatible (claude-code accepts any
    // hyphenated 8-4-4-4-12 hex string, NOT only v4 — we honor that).
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
        .expect("uuid regex compiles")
});

/// Returns true if `s` matches the byte-locked claude-code UUID regex.
#[must_use]
pub fn validate_uuid(s: &str) -> bool {
    UUID_RE.is_match(s)
}
