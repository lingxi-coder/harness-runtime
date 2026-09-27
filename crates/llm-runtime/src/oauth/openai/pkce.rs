//! PKCE (RFC 7636) verifier + S256 challenge generation for the
//! Authorization Code flow used by the `OpenAI` / `ChatGPT` OAuth client.
//!
//! The verifier is a 32-byte random string base64url-encoded (no padding);
//! the challenge is `SHA-256(verifier)` similarly encoded.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};

/// Generate a `(code_verifier, code_challenge)` pair.
///
/// The challenge is sent in the authorize URL; the verifier is held by the
/// client and supplied at the token-exchange step. Both are URL-safe base64
/// without padding.
#[must_use]
pub fn generate_pkce() -> (String, String) {
    let mut rng = rand::rng();
    let bytes: [u8; 32] = std::array::from_fn(|_| rng.random::<u8>());
    let verifier = URL_SAFE_NO_PAD.encode(bytes);

    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(h.finalize());

    (verifier, challenge)
}

/// Generate a 16-byte CSRF state token, base64url-encoded (no padding).
///
/// Sent in the authorize URL and validated against the value echoed back in
/// the OAuth redirect callback.
#[must_use]
pub fn generate_state_token() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 16] = std::array::from_fn(|_| rng.random::<u8>());
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_and_challenge_distinct_per_call() {
        let (v1, _) = generate_pkce();
        let (v2, _) = generate_pkce();
        assert_ne!(v1, v2);
    }

    #[test]
    fn challenge_is_url_safe_base64() {
        let (_, c) = generate_pkce();
        assert!(!c.contains('+'));
        assert!(!c.contains('/'));
        assert!(!c.contains('='));
    }
}
