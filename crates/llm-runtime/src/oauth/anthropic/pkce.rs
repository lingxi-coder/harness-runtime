//! Shared OAuth PKCE and CSRF generation from the SDK.

pub use lingxi_llm_client::auth::oauth::pkce::{generate_pkce, generate_state_token};

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
