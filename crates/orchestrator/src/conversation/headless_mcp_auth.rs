//! SDK authentication controls share the session's ordinary MCP OAuth owner.
use super::*;

impl ConversationOrchestrator {
    /// Start SDK OAuth without invoking a host browser. Exact authored UTF-16
    /// identities select both the registry entry and its credential namespace.
    pub async fn authenticate_sdk_mcp(
        &self,
        server: &Utf16JsonProjection,
        redirect_uri: Option<String>,
        redirect_only: bool,
    ) -> Result<Value, mcp::registry::SdkMcpAuthenticationError> {
        let units = server.string_units("").unwrap_or_default();
        let registry =
            self.mcp_registry
                .as_ref()
                .ok_or_else(|| mcp::registry::SdkMcpAuthenticationError {
                    message: format!(
                        "Server not found: {}",
                        mcp::normalization::sanitize_control_name_units(&units)
                    ),
                    error_code: None,
                })?;
        registry
            .start_sdk_authentication(&units, redirect_uri, redirect_only)
            .await
    }

    /// Complete the active SDK OAuth grant; the native pasted-callback route
    /// acknowledges completion independently of reconnecting the MCP server.
    pub async fn submit_sdk_mcp_oauth_callback(
        &self,
        server: &Utf16JsonProjection,
        callback: &str,
    ) -> Result<(), mcp::registry::SdkMcpAuthenticationError> {
        let units = server.string_units("").unwrap_or_default();
        let registry =
            self.mcp_registry
                .as_ref()
                .ok_or_else(|| mcp::registry::SdkMcpAuthenticationError {
                    message: format!(
                        "No active OAuth flow for server: {}",
                        mcp::normalization::sanitize_control_name_units(&units)
                    ),
                    error_code: None,
                })?;
        registry.submit_sdk_oauth_callback(&units, callback).await
    }

    /// The print session stops its callback listeners and detached OAuth work
    /// before retaining the shared registry for any other host work.
    pub fn cancel_sdk_mcp_authentication(&self) {
        if let Some(registry) = self.mcp_registry.as_ref() {
            registry.cancel_sdk_authentications();
        }
    }
}
