//! TLS/SSL certificate error classification and fix hint.
//!
//! Parity: claude-code 2.1.201 `JF` (cause-chain classifier), the `Gyo`/`bBp`
//! Node/OpenSSL error-code sets, and `YLe` (the user-facing fix hint).
//!
//! ## Why this exists
//!
//! A TLS/certificate failure (expired cert, self-signed cert, corporate
//! TLS-intercepting proxy, etc.) is **not** transient — retrying it just burns
//! the entire retry budget waiting on a handshake that will never succeed.
//! claude-code walks the error's `cause` chain, and when it finds a Node error
//! whose `code` is in the `bBp` SSL-code set it marks the error `isSSLError`,
//! short-circuits the retry loop to terminal, and surfaces the `YLe` hint
//! (which points the user at `NODE_EXTRA_CA_CERTS` / `/doctor`).
//!
//! In this port [`lingxi_core::host::HttpError`] is string-typed (there is no structured
//! `error.code` on the source chain), so the classifier scans the rendered
//! error text for any of the known code tokens instead of doing an exact
//! `Set.has(code)` lookup on a numeric/string code field. The token set and
//! the hint copy are byte-faithful to the oracle.

/// Certificate-verification error codes (`Gyo`).
///
/// These are Node.js / OpenSSL X.509 chain-verification codes. They are wire
/// diagnostic tokens (not brand tokens), so they are kept verbatim.
///
/// Byte-faithful to the 2.1.201 binary's `Gyo=new Set([...])`.
pub const CERT_CODES: &[&str] = &[
    "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
    "UNABLE_TO_GET_ISSUER_CERT",
    "UNABLE_TO_GET_ISSUER_CERT_LOCALLY",
    "CERT_SIGNATURE_FAILURE",
    "CERT_NOT_YET_VALID",
    "CERT_HAS_EXPIRED",
    "CERT_REVOKED",
    "CERT_REJECTED",
    "CERT_UNTRUSTED",
    "DEPTH_ZERO_SELF_SIGNED_CERT",
    "SELF_SIGNED_CERT_IN_CHAIN",
    "CERT_CHAIN_TOO_LONG",
    "PATH_LENGTH_EXCEEDED",
    "ERR_TLS_CERT_ALTNAME_INVALID",
    "HOSTNAME_MISMATCH",
];

/// SSL/TLS protocol-level error codes added to [`CERT_CODES`] to form the full
/// SSL-error set (`bBp = Gyo ∪ {handshake/protocol codes}`).
///
/// Byte-faithful to the 2.1.201 binary's
/// `bBp=new Set([...Gyo,"ERR_TLS_HANDSHAKE_TIMEOUT",...])`.
pub const SSL_PROTOCOL_CODES: &[&str] = &[
    "ERR_TLS_HANDSHAKE_TIMEOUT",
    "ERR_SSL_WRONG_VERSION_NUMBER",
    "ERR_SSL_DECRYPTION_FAILED_OR_BAD_RECORD_MAC",
];

/// Whether `code` is in the certificate-verification set (`Gyo.has(code)`).
#[must_use]
pub fn is_cert_code(code: &str) -> bool {
    CERT_CODES.contains(&code)
}

/// Whether `code` is in the full SSL-error set (`bBp.has(code)`).
#[must_use]
pub fn is_ssl_code(code: &str) -> bool {
    CERT_CODES.contains(&code) || SSL_PROTOCOL_CODES.contains(&code)
}

/// Scan an error message / cause-chain string for a known SSL error code and
/// return the matched code, or `None` when no SSL code is present.
///
/// Parity: the terminal decision hinges on `JF(e)?.isSSLError`, i.e. whether the
/// cause chain carries a `code` in the `bBp` set. Because this port's
/// [`lingxi_core::host::HttpError`] is string-typed, we substring-match the code tokens
/// against the rendered error text.
///
/// Codes are matched **longest token first** so that a message carrying
/// `UNABLE_TO_GET_ISSUER_CERT_LOCALLY` reports that specific code rather than
/// its prefix `UNABLE_TO_GET_ISSUER_CERT` (both are in the set, so `isSSLError`
/// is `true` either way — this only picks the most specific code to report).
#[must_use]
pub fn detect_ssl_code(message: &str) -> Option<&'static str> {
    let mut codes: Vec<&'static str> = CERT_CODES
        .iter()
        .chain(SSL_PROTOCOL_CODES.iter())
        .copied()
        .collect();
    // Longest token first: a longer code that contains a shorter one as a
    // prefix (e.g. `..._LOCALLY`) must win.
    codes.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    codes.into_iter().find(|code| message.contains(code))
}

/// Build the SSL certificate fix hint for `code` (`YLe`).
///
/// Byte-faithful to the 2.1.201 binary's `YLe`:
/// `SSL certificate error (${t.code}). If you are behind a corporate proxy or
/// TLS-intercepting firewall, set NODE_EXTRA_CA_CERTS to your CA bundle path, or
/// ask IT to allowlist *.anthropic.com. Run /doctor for details.`
///
/// `NODE_EXTRA_CA_CERTS`, `*.anthropic.com`, and `/doctor` are preserved
/// verbatim — `NODE_EXTRA_CA_CERTS` is the Node runtime env var the user must
/// actually set, `*.anthropic.com` is the API host to allowlist, and `/doctor`
/// is a real slash command. None are brand tokens subject to the swap.
#[must_use]
pub fn ssl_hint(code: &str) -> String {
    format!(
        "SSL certificate error ({code}). If you are behind a corporate proxy or \
TLS-intercepting firewall, set NODE_EXTRA_CA_CERTS to your CA bundle path, or \
ask IT to allowlist *.anthropic.com. Run /doctor for details."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_set_matches_oracle_gyo() {
        // 15 codes in Gyo (2.1.201 binary).
        assert_eq!(CERT_CODES.len(), 15);
        for code in [
            "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
            "UNABLE_TO_GET_ISSUER_CERT",
            "UNABLE_TO_GET_ISSUER_CERT_LOCALLY",
            "CERT_SIGNATURE_FAILURE",
            "CERT_NOT_YET_VALID",
            "CERT_HAS_EXPIRED",
            "CERT_REVOKED",
            "CERT_REJECTED",
            "CERT_UNTRUSTED",
            "DEPTH_ZERO_SELF_SIGNED_CERT",
            "SELF_SIGNED_CERT_IN_CHAIN",
            "CERT_CHAIN_TOO_LONG",
            "PATH_LENGTH_EXCEEDED",
            "ERR_TLS_CERT_ALTNAME_INVALID",
            "HOSTNAME_MISMATCH",
        ] {
            assert!(is_cert_code(code), "{code} must be a cert code");
            assert!(is_ssl_code(code), "{code} must be an SSL code");
        }
    }

    #[test]
    fn ssl_set_is_gyo_plus_three_protocol_codes() {
        // bBp = Gyo ∪ {handshake/protocol}. Protocol codes are SSL but not cert.
        for code in [
            "ERR_TLS_HANDSHAKE_TIMEOUT",
            "ERR_SSL_WRONG_VERSION_NUMBER",
            "ERR_SSL_DECRYPTION_FAILED_OR_BAD_RECORD_MAC",
        ] {
            assert!(is_ssl_code(code), "{code} must be an SSL code");
            assert!(
                !is_cert_code(code),
                "{code} is protocol-only, not a cert code"
            );
        }
    }

    #[test]
    fn non_ssl_codes_are_rejected() {
        for code in [
            "ECONNREFUSED",
            "ENOTFOUND",
            "ETIMEDOUT",
            "ConnectionClosed",
            "",
        ] {
            assert!(!is_ssl_code(code), "{code} must not be an SSL code");
        }
    }

    #[test]
    fn detect_scans_error_text_for_code() {
        assert_eq!(
            detect_ssl_code("connection failed: certificate has expired (CERT_HAS_EXPIRED)"),
            Some("CERT_HAS_EXPIRED")
        );
        assert_eq!(
            detect_ssl_code("tls handshake: SELF_SIGNED_CERT_IN_CHAIN"),
            Some("SELF_SIGNED_CERT_IN_CHAIN")
        );
        assert_eq!(
            detect_ssl_code("ERR_SSL_WRONG_VERSION_NUMBER"),
            Some("ERR_SSL_WRONG_VERSION_NUMBER")
        );
    }

    #[test]
    fn detect_prefers_most_specific_code() {
        // `..._LOCALLY` contains `UNABLE_TO_GET_ISSUER_CERT` as a prefix; the
        // longer, more specific code must be reported.
        assert_eq!(
            detect_ssl_code("verify error: UNABLE_TO_GET_ISSUER_CERT_LOCALLY at depth 0"),
            Some("UNABLE_TO_GET_ISSUER_CERT_LOCALLY")
        );
    }

    #[test]
    fn detect_returns_none_for_non_ssl_transport_error() {
        assert_eq!(detect_ssl_code("connection refused: ECONNREFUSED"), None);
        assert_eq!(
            detect_ssl_code("dns error: ENOTFOUND api.example.com"),
            None
        );
        assert_eq!(detect_ssl_code(""), None);
    }

    #[test]
    fn hint_matches_oracle_copy() {
        assert_eq!(
            ssl_hint("CERT_HAS_EXPIRED"),
            "SSL certificate error (CERT_HAS_EXPIRED). If you are behind a corporate \
proxy or TLS-intercepting firewall, set NODE_EXTRA_CA_CERTS to your CA bundle \
path, or ask IT to allowlist *.anthropic.com. Run /doctor for details."
        );
    }
}
