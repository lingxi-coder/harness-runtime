//! Parse `OpenAI` `id_token` JWT claims (no signature verification — the token came
//! straight from our own token exchange over TLS). Mirrors codex `token_data.rs`
//! claim names.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;

/// Claims we care about from the `OpenAI` `id_token`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdTokenClaims {
    /// `ChatGPT` workspace/account id → `ChatGPT-Account-ID` header.
    pub account_id: Option<String>,
    /// `FedRAMP` account flag → `X-OpenAI-Fedramp` header.
    pub fedramp: bool,
    /// User email (best-effort, for display).
    pub email: Option<String>,
}

/// Parse the JWT's payload segment and extract the claims. Returns `None` if the
/// token is malformed (not three dot-separated base64url segments / bad JSON).
#[must_use]
pub fn parse_id_token(jwt: &str) -> Option<IdTokenClaims> {
    let payload_b64 = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let auth = v.get("https://api.openai.com/auth");
    let account_id = auth
        .and_then(|a| a.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let fedramp = auth
        .and_then(|a| a.get("chatgpt_account_is_fedramp"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let email = v.get("email").and_then(Value::as_str).map(str::to_string);
    Some(IdTokenClaims {
        account_id,
        fedramp,
        email,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_jwt() -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123","chatgpt_account_is_fedramp":false},"email":"u@example.com"}"#,
        );
        format!("{header}.{payload}.sig")
    }
    #[test]
    fn parses_account_id_and_fedramp() {
        let claims = parse_id_token(&fixture_jwt()).expect("parse");
        assert_eq!(claims.account_id.as_deref(), Some("acc_123"));
        assert!(!claims.fedramp);
        assert_eq!(claims.email.as_deref(), Some("u@example.com"));
    }
    #[test]
    fn malformed_jwt_returns_none() {
        assert!(parse_id_token("not-a-jwt").is_none());
    }
}
