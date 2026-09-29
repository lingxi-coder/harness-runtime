//! Host selection of the GitHub OAuth application and editor identity.

pub use lingxi_llm_client::auth::oauth::copilot::{
    exchange_copilot_token, CopilotLogin, DeviceCodeResponse, ExchangeIdentity, ExchangedToken,
    PollOutcome, COPILOT_CLIENT_ID, COPILOT_TOKEN_EXCHANGE_URL, COPILOT_TOKEN_REFRESH_SKEW_SECS,
    DEFAULT_GITHUB_DOMAIN,
};

#[must_use]
pub fn copilot_client_id() -> String {
    std::env::var("LINGXI_COPILOT_CLIENT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| COPILOT_CLIENT_ID.to_string())
}

pub const COPILOT_EDITOR_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
pub const COPILOT_EDITOR_VERSION: &str = "vscode/1.99.3";
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

pub const EXCHANGE_IDENTITY: ExchangeIdentity<'static> = ExchangeIdentity {
    user_agent: COPILOT_EDITOR_USER_AGENT,
    editor_version: COPILOT_EDITOR_VERSION,
    plugin_version: COPILOT_EDITOR_PLUGIN_VERSION,
    integration_id: COPILOT_INTEGRATION_ID,
};
