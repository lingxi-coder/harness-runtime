//! Semantic session services. Tools never construct provider HTTP requests.
use lingxi_core::types::utf16_json::Utf16JsonProjection;
pub use lingxi_llm_client::hosted_search::{parse_response_content, SearchResultEntry};
pub use lingxi_llm_client::protocol::JsonPayload;
/// Exact MCP token counting supplied by the active provider route.
#[async_trait::async_trait]
pub trait McpTokenCounter: Send + Sync {
    /// Count model-facing MCP content exactly when the active route supports
    /// it. `Ok(None)` means the route has no exact token-count endpoint.
    async fn count_mcp_content_tokens(
        &self,
        model: &str,
        content: &Utf16JsonProjection,
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
    pub query: Utf16JsonProjection,
    /// Domain allowlist supplied by the caller.
    pub allowed_domains: Vec<Utf16JsonProjection>,
    /// Domain denylist supplied by the caller.
    pub blocked_domains: Vec<Utf16JsonProjection>,
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
#[error("{}", .message.string_at("").unwrap_or_default())]
pub struct HostedSearchError {
    /// Safe caller-facing diagnostic.
    pub message: Utf16JsonProjection,
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
