//! Synchronous GitHub Copilot request authenticator + redacting token wrapper.
//!
//! [`CopilotAuthenticator::apply`] is pure header injection so it fits the
//! synchronous [`Authenticator`] trait: it stamps the Copilot header set and
//! uses [`CopilotSecret`] as the bearer.
//!
//! `api.githubcopilot.com` requires a SHORT-LIVED Copilot token, not the raw
//! GitHub OAuth-App token. That token is minted by the asynchronous exchange in
//! [`crate::copilot::login::exchange_copilot_token`] and must be cached with its
//! `expires_at` and re-exchanged near expiry by the host. The authenticator is
//! deliberately credential-agnostic: it bearers whatever [`CopilotSecret`] it is
//! constructed with, so the host wires the freshly-exchanged token here. See
//! `login.rs` for the exchange + the host-wiring contract.

use crate::{Authenticator, LlmError, ProviderRequest};

/// `X-GitHub-Api-Version` header value sent to GitHub Copilot.
pub const COPILOT_API_VERSION: &str = "2026-06-01";
/// `User-Agent` sent to GitHub Copilot.
pub const COPILOT_USER_AGENT: &str = "LingXi-Code";
/// `Copilot-Integration-Id` header — Copilot rejects requests without a known
/// integration id. `vscode-chat` is the integration id used by the OpenAI-
/// compatible chat endpoint.
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";
/// `Editor-Version` header value. Copilot expects an `<editor>/<version>` token;
/// we send a stable identifier for this client.
pub const COPILOT_EDITOR_VERSION: &str = "LingXi-Code/1.0";
/// `Editor-Plugin-Version` header value (the Copilot plugin/extension version).
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "LingXi-Code/1.0";

/// GitHub OAuth token used directly as the Copilot bearer credential.
///
/// The `Debug` impl is redacting so the token never reaches logs or errors.
#[derive(Clone)]
pub struct CopilotSecret(String);

impl CopilotSecret {
    /// Wrap a GitHub OAuth token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// **Plan 3c frozen-crate (§10) EXCEPTION — documented deviation.** Expose the
    /// raw GitHub OAuth token so the host `/connect` device-flow driver can persist
    /// it to the keychain under `github-copilot`. This is the ONLY reader; the
    /// `Debug` impl stays redacting. `#[doc(hidden)]` so it is not part of the
    /// public surface and is only reachable by the engine that already drives the
    /// Copilot login. Do not use for logging or display.
    #[doc(hidden)]
    #[must_use]
    pub fn token_for_storage(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CopilotSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CopilotSecret(<redacted>)")
    }
}

/// Authenticator for GitHub Copilot's OpenAI-compatible endpoint. Injects the
/// Copilot header set and uses the GitHub OAuth token directly as the bearer.
#[derive(Debug, Clone)]
pub struct CopilotAuthenticator {
    secret: CopilotSecret,
}

impl CopilotAuthenticator {
    /// Create an authenticator from a GitHub OAuth token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            secret: CopilotSecret::new(token),
        }
    }
}

impl Authenticator for CopilotAuthenticator {
    fn apply(&self, mut request: ProviderRequest) -> Result<ProviderRequest, LlmError> {
        lingxi_llm_client::auth::header_policy::copilot(
            &mut request.headers,
            &self.secret.0,
            COPILOT_USER_AGENT,
            COPILOT_EDITOR_VERSION,
            COPILOT_EDITOR_PLUGIN_VERSION,
        );
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn injects_copilot_headers_and_strips_x_api_key() {
        let mut request = ProviderRequest::post_json(
            "https://api.githubcopilot.com/chat/completions",
            json!({ "model": "gpt-5.4-nano" }),
        );
        request
            .headers
            .insert("x-api-key".to_string(), "leftover".to_string());

        let signed = CopilotAuthenticator::new("ght_token")
            .apply(request)
            .expect("applies");

        assert_eq!(
            signed.headers.get("Authorization"),
            Some(&"Bearer ght_token".to_string())
        );
        assert_eq!(
            signed.headers.get("X-GitHub-Api-Version"),
            Some(&"2026-06-01".to_string())
        );
        assert_eq!(
            signed.headers.get("Openai-Intent"),
            Some(&"conversation-edits".to_string())
        );
        assert_eq!(
            signed.headers.get("User-Agent"),
            Some(&"LingXi-Code".to_string())
        );
        assert_eq!(
            signed.headers.get("x-initiator"),
            Some(&"agent".to_string())
        );
        assert_eq!(
            signed.headers.get("Copilot-Integration-Id"),
            Some(&"vscode-chat".to_string())
        );
        assert_eq!(
            signed.headers.get("Editor-Version"),
            Some(&"LingXi-Code/1.0".to_string())
        );
        assert_eq!(
            signed.headers.get("Editor-Plugin-Version"),
            Some(&"LingXi-Code/1.0".to_string())
        );
        assert!(!signed.headers.contains_key("x-api-key"));
    }

    #[test]
    fn token_for_storage_returns_raw_token_for_persistence() {
        // Frozen-crate (§10) exception: the /connect device-flow MUST persist the
        // GitHub token under `github-copilot`. The Debug stays redacting; only
        // this explicit, doc-hidden accessor exposes the raw token.
        let s = CopilotSecret::new("ght_live_token");
        assert_eq!(s.token_for_storage(), "ght_live_token");
        // Debug is still redacting (no regression).
        assert!(!format!("{s:?}").contains("ght_live_token"));
    }

    #[test]
    fn debug_does_not_leak_token() {
        let dbg_secret = format!("{:?}", CopilotSecret::new("supersecret"));
        assert!(!dbg_secret.contains("supersecret"));
        let dbg_auth = format!("{:?}", CopilotAuthenticator::new("supersecret"));
        assert!(!dbg_auth.contains("supersecret"));
    }
}
