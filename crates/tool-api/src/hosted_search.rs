//! Semantic session services. Tools never construct provider HTTP requests.
pub use lingxi_llm_client::hosted_search::{parse_response_content, SearchResultEntry};
/// Exact MCP token counting supplied by the active provider route.
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
/// A provider-hosted search requested by the session.
pub struct HostedSearchRequest {
    /// Active model identifier.
    pub model: String,
    /// Explicit provider profile, when selected.
    pub profile: Option<String>,
    /// Search query.
    pub query: String,
    /// Domain allowlist supplied by the caller.
    pub allowed_domains: Vec<String>,
    /// Domain denylist supplied by the caller.
    pub blocked_domains: Vec<String>,
}
/// Search results and provider-reported usage.
pub struct HostedSearchOutput {
    /// Parsed search entries.
    pub results: Vec<SearchResultEntry>,
    /// Number of billed searches.
    pub searches: u64,
    /// Reported input token count.
    pub input_tokens: u64,
    /// Reported output token count.
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
/// Session service for provider-hosted web search.
#[async_trait::async_trait]
pub trait HostedWebSearchClient: Send + Sync {
    /// Whether the selected route supports hosted search.
    fn supports(&self, model: &str, profile: Option<&str>) -> bool;
    /// Whether the route supports this request's additional constraints.
    fn supports_request(&self, request: &HostedSearchRequest) -> bool {
        self.supports(&request.model, request.profile.as_deref())
    }
    /// Execute the search and forward progress signals.
    async fn search(
        &self,
        request: HostedSearchRequest,
        progress: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Result<HostedSearchOutput, HostedSearchError>;
}
