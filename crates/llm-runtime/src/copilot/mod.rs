//! GitHub Copilot provider support: device-flow login and host credential lifecycle.

pub mod auth;
pub mod login;

pub use auth::{
    CopilotSecret, COPILOT_API_VERSION, COPILOT_EDITOR_PLUGIN_VERSION, COPILOT_EDITOR_VERSION,
    COPILOT_INTEGRATION_ID, COPILOT_USER_AGENT,
};
pub use login::{
    exchange_copilot_token, CopilotHttp, CopilotLogin, DeviceCodeResponse, ExchangedToken,
    PollOutcome, COPILOT_CLIENT_ID, COPILOT_TOKEN_EXCHANGE_URL, COPILOT_TOKEN_REFRESH_SKEW_SECS,
    DEFAULT_GITHUB_DOMAIN,
};
