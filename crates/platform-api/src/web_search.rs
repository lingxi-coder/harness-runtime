//! Runtime configuration seam for provider-agnostic client-side WebSearch.

use async_trait::async_trait;

/// Resolved `/web` runtime configuration visible to the WebSearch tool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebSearchRuntimeConfig {
    /// Active provider string (`auto`, `duckduckgo`, `tavily`, `brave`, `searxng`).
    pub provider: Option<String>,
    /// SearXNG base URL from settings, when configured.
    pub searxng_url: Option<String>,
    /// Tavily key from the secure store, when configured.
    pub tavily_key: Option<String>,
    /// Brave key from the secure store, when configured.
    pub brave_key: Option<String>,
}

/// Async loader for `/web` settings + secure credentials.
#[async_trait]
pub trait WebSearchConfigProvider: Send + Sync {
    /// Load current runtime web-search configuration.
    async fn load_web_search_config(&self) -> WebSearchRuntimeConfig;
}
