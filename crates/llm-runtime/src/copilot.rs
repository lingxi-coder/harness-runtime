//! Host Copilot application selection and editor identity.
//! Device login, token exchange, secrets and protocol headers belong to llm-client.
use lingxi_llm_client::auth::oauth::copilot::{ExchangeIdentity, COPILOT_CLIENT_ID};

pub const COPILOT_USER_AGENT: &str = "LingXi-Code";
pub const COPILOT_EDITOR_VERSION: &str = "LingXi-Code/1.0";
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "LingXi-Code/1.0";

#[must_use]
pub fn copilot_client_id() -> String {
    std::env::var("LINGXI_COPILOT_CLIENT_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| COPILOT_CLIENT_ID.to_string())
}

/// Identity selected by the host for the Copilot token exchange application.
pub const EXCHANGE_IDENTITY: ExchangeIdentity<'static> = ExchangeIdentity {
    user_agent: "GitHubCopilotChat/0.26.7",
    editor_version: "vscode/1.99.3",
    plugin_version: "copilot-chat/0.26.7",
    integration_id: "vscode-chat",
};
