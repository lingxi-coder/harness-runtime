//! Host validators + canonicalizers (`parent-proxy.js:372-410`). Security-
//! critical: they make the allowlist agree with what `getaddrinfo()` dials
//! (denylist-evasion defense) and reject CRLF/null/zone-id injection.

use std::net::IpAddr;

/// Strip surrounding `[ ]` from a bracketed IPv6 literal.
#[must_use]
pub fn strip_brackets(h: &str) -> String {
    h.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(h)
        .to_string()
}

/// True if `h` parses as an IP literal.
fn is_ip(h: &str) -> bool {
    h.parse::<IpAddr>().is_ok()
}

/// `isValidHost` (`parent-proxy.js:372-384`): non-empty, ≤255 chars; reject
/// zone IDs (`%`); accept IP literals; else require the DNS label charset
/// `[A-Za-z0-9._-]+` (underscore allowed for `_dmarc` etc.). This rejects
/// control chars / CRLF / null / spaces.
#[must_use]
pub fn is_valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 255 {
        return false;
    }
    let bare = strip_brackets(h);
    if bare.contains('%') {
        return false;
    }
    if is_ip(&bare) {
        return true;
    }
    bare.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `canonicalizeHost` (`parent-proxy.js:396-410`): normalize via the WHATWG URL
/// parser so allowlist comparisons match `getaddrinfo()` — collapses `inet_aton`
/// shorthand, hex/octal octets, IPv6 compression, trailing dots, case,
/// brackets. `None` if the input is not a valid URL host.
#[must_use]
pub fn canonicalize_host(h: &str) -> Option<String> {
    let bare = strip_brackets(h);
    // WHATWG parses bare IPv6 only when bracketed.
    let bracketed = if matches!(bare.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
        format!("[{bare}]")
    } else {
        bare.clone()
    };
    let url = url::Url::parse(&format!("http://{bracketed}/")).ok()?;
    let host = url.host_str()?;
    Some(strip_brackets(host).trim_end_matches('.').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_host_rejects_injection_and_zone_ids() {
        // parent-proxy.js:372-384
        assert!(is_valid_host("example.com"));
        assert!(is_valid_host("a.b-c_d.example.com"));
        assert!(is_valid_host("127.0.0.1"));
        assert!(is_valid_host("::1"));
        assert!(!is_valid_host(""));
        assert!(!is_valid_host(&"a".repeat(256)));
        assert!(!is_valid_host("evil.com\u{0}.allowed.com")); // null byte
        assert!(!is_valid_host("evil.com\r\n.allowed.com")); // CRLF
        assert!(!is_valid_host("fe80::1%eth0")); // zone id
        assert!(!is_valid_host("has space.com"));
    }

    #[test]
    fn strip_brackets_unwraps_ipv6() {
        assert_eq!(strip_brackets("[::1]"), "::1");
        assert_eq!(strip_brackets("example.com"), "example.com");
    }

    #[test]
    fn canonicalize_host_matches_getaddrinfo() {
        // parent-proxy.js:396-410 — inet_aton shorthand, hex/octal, ipv6, trailing dot
        assert_eq!(canonicalize_host("127.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(
            canonicalize_host("2130706433").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            canonicalize_host("0x7f.0.0.1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(canonicalize_host("0:0:0:0:0:0:0:1").as_deref(), Some("::1"));
        assert_eq!(
            canonicalize_host("Example.COM.").as_deref(),
            Some("example.com")
        );
        assert_eq!(canonicalize_host("[::1]").as_deref(), Some("::1"));
        assert!(canonicalize_host("evil\u{0}.com").is_none());
    }
}
