//! Static configuration for the Claude.ai OAuth client.
//!
//! See spec §30.1. Endpoints mirror the canonical claude-code reference; the
//! redirect URI is a `http://127.0.0.1:{port}/callback` loopback that the
//! caller binds before opening the browser.

use serde::{Deserialize, Serialize};

/// Endpoint and identifier set for the Authorization Code flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeAiOAuthConfig {
    /// Browser-facing authorization endpoint.
    pub authorization_endpoint: String,
    /// Backchannel token-exchange / refresh endpoint.
    pub token_endpoint: String,
    /// Backchannel revocation endpoint.
    pub revocation_endpoint: String,
    /// Profile endpoint used to fetch the authenticated user record.
    pub profile_endpoint: String,
    /// OAuth client ID registered with login.claude.ai.
    pub client_id: String,
    /// Loopback redirect URI registered for the local CLI flow.
    pub redirect_uri: String,
    /// Hosted "code page" redirect URI for the MANUAL copy-paste flow
    /// (claude-code `MANUAL_REDIRECT_URL`): the landing page displays
    /// `code#state` for the user to paste back into the CLI when the loopback
    /// redirect can't fire (no browser / remote shell). `serde(default)` keeps
    /// previously-serialized configs loading.
    ///
    /// It is NOT interchangeable with [`Self::redirect_uri`]: claude selects
    /// one for the authorize URL AND the same one for the token exchange
    /// (`redirect_uri: o ? MANUAL_REDIRECT_URL : http://localhost:…/callback`).
    /// Exchanging a manually-pasted code against the loopback redirect fails.
    #[serde(default = "default_manual_redirect_uri")]
    pub manual_redirect_uri: String,
    /// Scopes requested in the authorize URL.
    pub scopes: Vec<String>,
}

/// claude-code `MANUAL_REDIRECT_URL` (`constants/oauth.ts`) — shared by the
/// claude.ai and Console variants of the OAuth application.
fn default_manual_redirect_uri() -> String {
    "https://platform.claude.com/oauth/code/callback".into()
}

/// Claude Code 2.1.217's normal interactive OAuth scopes, in wire order.
pub const CLAUDE_CODE_OAUTH_SCOPES: &[&str] = &[
    "org:create_api_key",
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

/// Scope used by `setup-token`'s deliberately restricted one-year token.
pub const CLAUDE_CODE_INFERENCE_SCOPE: &str = "user:inference";

/// Claude Code's long-lived OAuth token duration (one year).
pub const LONG_LIVED_OAUTH_TOKEN_TTL_SECONDS: u64 = 31_536_000;

/// Refresh-token grant value used by Anthropic's OAuth endpoint.
pub const REFRESH_GRANT_TYPE: &str = "refresh_token";

impl ClaudeAiOAuthConfig {
    /// Default config — endpoints per claude-code reference, redirect URI
    /// targeted at a caller-supplied loopback `port`.
    #[must_use]
    pub fn default_with_port(port: u16) -> Self {
        Self {
            authorization_endpoint: "https://claude.com/cai/oauth/authorize".into(),
            token_endpoint: "https://platform.claude.com/v1/oauth/token".into(),
            revocation_endpoint: "https://platform.claude.com/v1/oauth/token/revoke".into(),
            profile_endpoint: "https://api.anthropic.com/api/oauth/profile".into(),
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into(),
            redirect_uri: format!("http://localhost:{port}/callback"),
            // claude `MANUAL_REDIRECT_URL` for the prod tier (2.1.220
            // @226014228), shared with the Console variant below.
            manual_redirect_uri: default_manual_redirect_uri(),
            scopes: CLAUDE_CODE_OAUTH_SCOPES
                .iter()
                .map(|s| (*s).into())
                .collect(),
        }
    }

    /// Anthropic Console/API-billing variant of the same OAuth application.
    #[must_use]
    pub fn console_with_port(port: u16) -> Self {
        Self {
            authorization_endpoint: "https://platform.claude.com/oauth/authorize".into(),
            ..Self::default_with_port(port)
        }
    }
}
