//! Host GitHub Copilot identity and redacting credential storage.
//!
//! The host exchanges and refreshes short-lived bearer tokens in `login`.
//! The SDK applies those credentials and the host identity to provider requests.

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

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_llm_client::auth::{apply_credential, ClientIdentity, CredentialRef};
    use serde_json::json;

    #[test]
    fn injects_copilot_headers_and_strips_x_api_key() {
        let mut request = lingxi_llm_client::HttpRequest {
            method: "POST".into(),
            url: "https://api.githubcopilot.com/chat/completions".into(),
            headers: vec![("x-api-key".into(), "leftover".into())],
            body: serde_json::to_vec(&json!({"model":"gpt-5.4-nano"}))
                .unwrap()
                .into(),
            timeout: None,
        };
        let profile = serde_json::from_value(json!({
            "provider_id":"github-copilot", "profile_name":"github-copilot",
            "base_url":"https://api.githubcopilot.com", "protocol":"open_ai_chat",
            "auth":"copilot_bearer", "models":[]
        }))
        .unwrap();
        apply_credential(
            &mut request,
            &profile,
            CredentialRef::Token("ght_token"),
            ClientIdentity {
                user_agent: COPILOT_USER_AGENT,
                editor_version: COPILOT_EDITOR_VERSION,
                plugin_version: COPILOT_EDITOR_PLUGIN_VERSION,
            },
            std::time::SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        let headers: std::collections::BTreeMap<_, _> = request.headers.into_iter().collect();

        assert_eq!(
            headers.get("Authorization"),
            Some(&"Bearer ght_token".to_string())
        );
        assert_eq!(
            headers.get("X-GitHub-Api-Version"),
            Some(&"2026-06-01".to_string())
        );
        assert_eq!(
            headers.get("Openai-Intent"),
            Some(&"conversation-edits".to_string())
        );
        assert_eq!(headers.get("User-Agent"), Some(&"LingXi-Code".to_string()));
        assert_eq!(headers.get("x-initiator"), Some(&"agent".to_string()));
        assert_eq!(
            headers.get("Copilot-Integration-Id"),
            Some(&"vscode-chat".to_string())
        );
        assert_eq!(
            headers.get("Editor-Version"),
            Some(&"LingXi-Code/1.0".to_string())
        );
        assert_eq!(
            headers.get("Editor-Plugin-Version"),
            Some(&"LingXi-Code/1.0".to_string())
        );
        assert!(!headers.contains_key("x-api-key"));
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
    }
}
