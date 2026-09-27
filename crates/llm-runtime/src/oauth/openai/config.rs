//! Static `OpenAI` OAuth endpoints + `client_id` + scopes + Codex backend URL.
//! Constants verified against codex `login/src/server.rs` and `model-provider-info`.

/// `OpenAI` OAuth configuration (issuer, `client_id`, endpoints, scopes, backend).
#[derive(Debug, Clone)]
pub struct OpenAiOAuthConfig {
    /// Base issuer URL (e.g. `https://auth.openai.com`).
    pub issuer: String,
    /// OAuth `client_id` registered with the issuer.
    pub client_id: String,
    /// Browser-facing authorization endpoint.
    pub authorize_url: String,
    /// Backchannel token-exchange / refresh endpoint.
    pub token_url: String,
    /// Device-code usercode issuance endpoint.
    pub device_usercode_url: String,
    /// Device-code token polling endpoint.
    pub device_token_url: String,
    /// User-visible device verification URL shown alongside the user code.
    pub device_verify_url: String,
    /// Space-separated OAuth scopes requested in the authorize URL.
    pub scopes: String,
    /// `ChatGPT` Codex backend base URL.
    pub codex_backend: String,
    /// Loopback redirect ports, in preference order (codex allowlist: 1455, then 1457).
    pub loopback_ports: [u16; 2],
    /// `AuthAPI` base URL for PAT whoami + account endpoints.
    pub authapi_base_url: String,
}

impl Default for OpenAiOAuthConfig {
    fn default() -> Self {
        let issuer = "https://auth.openai.com".to_string();
        Self {
            authorize_url: format!("{issuer}/oauth/authorize"),
            token_url: format!("{issuer}/oauth/token"),
            device_usercode_url: format!("{issuer}/api/accounts/deviceauth/usercode"),
            device_token_url: format!("{issuer}/api/accounts/deviceauth/token"),
            device_verify_url: format!("{issuer}/codex/device"),
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
            scopes: "openid profile email offline_access api.connectors.read api.connectors.invoke"
                .to_string(),
            codex_backend: "https://chatgpt.com/backend-api/codex".to_string(),
            loopback_ports: [1455, 1457],
            authapi_base_url: format!("{issuer}/api/accounts"),
            issuer,
        }
    }
}

impl OpenAiOAuthConfig {
    /// The loopback redirect URI for a chosen port.
    #[must_use]
    pub fn redirect_uri(&self, port: u16) -> String {
        format!("http://localhost:{port}/auth/callback")
    }

    /// The PAT `whoami` endpoint (resolves `account_id` / fedramp for a PAT).
    #[must_use]
    pub fn whoami_url(&self) -> String {
        format!(
            "{}/v1/user-auth-credential/whoami",
            self.authapi_base_url.trim_end_matches('/')
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_has_authapi_and_whoami() {
        let c = OpenAiOAuthConfig::default();
        assert_eq!(c.authapi_base_url, "https://auth.openai.com/api/accounts");
        assert_eq!(
            c.whoami_url(),
            "https://auth.openai.com/api/accounts/v1/user-auth-credential/whoami"
        );
    }
    #[test]
    fn config_has_codex_constants() {
        let c = OpenAiOAuthConfig::default();
        assert_eq!(c.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(c.token_url, "https://auth.openai.com/oauth/token");
        assert_eq!(c.authorize_url, "https://auth.openai.com/oauth/authorize");
        assert!(c.scopes.contains("offline_access"));
        assert_eq!(c.codex_backend, "https://chatgpt.com/backend-api/codex");
        assert_eq!(c.loopback_ports, [1455, 1457]);
        assert_eq!(c.redirect_uri(1455), "http://localhost:1455/auth/callback");
    }
}
