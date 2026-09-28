//! Semantic session services. Tools never construct provider HTTP requests.
pub use lingxi_llm_client::hosted_search::{parse_response_content, SearchResultEntry};
#[async_trait::async_trait]
pub trait McpTokenCounter: Send + Sync {
    /// Count model-facing MCP content exactly when the active route supports
    /// it. `Ok(None)` means the route has no exact token-count endpoint.
    async fn count_mcp_content_tokens(
        &self,
        model: &str,
        content: &serde_json::Value,
    ) -> Result<Option<u64>, String>;
}
#[derive(Clone, Debug)]
pub struct HostedSearchRequest {
    pub model: String,
    pub profile: Option<String>,
    pub query: String,
    pub allowed_domains: Vec<String>,
    pub blocked_domains: Vec<String>,
}
pub struct HostedSearchOutput {
    pub results: Vec<SearchResultEntry>,
    pub searches: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
/// Provider-neutral failure information retained for tool telemetry.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct HostedSearchError {
    /// Safe caller-facing diagnostic.
    pub message: String,
    /// Provider response status when known.
    pub http_status: Option<u16>,
    /// Whether a host or transport deadline expired.
    pub timeout: bool,
}
#[async_trait::async_trait]
pub trait HostedWebSearchClient: Send + Sync {
    fn supports(&self, model: &str, profile: Option<&str>) -> bool;
    fn supports_request(&self, request: &HostedSearchRequest) -> bool {
        self.supports(&request.model, request.profile.as_deref())
    }
    async fn search(
        &self,
        request: HostedSearchRequest,
        progress: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<HostedSearchOutput, HostedSearchError>;
}
