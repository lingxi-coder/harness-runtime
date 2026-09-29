//! Anthropic OAuth provider configuration reexported from the SDK.

pub use lingxi_llm_client::auth::oauth::anthropic::{
    ClaudeAiOAuthConfig, CLAUDE_CODE_INFERENCE_SCOPE, CLAUDE_CODE_OAUTH_SCOPES,
    LONG_LIVED_OAUTH_TOKEN_TTL_SECONDS, REFRESH_GRANT_TYPE,
};
