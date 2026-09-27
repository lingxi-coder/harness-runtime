//! Minimal Anthropic request builder for the WebSearch tool path.
//!
//! Replaces the `AnthropicProvider::build_request` seam formerly used
//! by `BuiltinToolContext.provider` + `WebSearchTool`. Only the non-streaming
//! `POST /v1/messages` request assembly is needed here; all retry, OAuth,
//! telemetry, and streaming logic lives in `llm-runtime` (used by the
//! main engine path via `orchestrator`).
//!
//! **Origin:** ported 1:1 from `AnthropicProvider::build_request`
//! (lingxi-code/api-client/src/anthropic.rs, deleted in Plan 3b). The exact
//! header set and URL join semantics are preserved byte-for-byte so existing
//! wire tests pass unchanged.

use protocol::{HttpMethod, HttpRequest};
use std::sync::Arc;

/// Session provider seam used by MCP's large-result guard for the exact
/// token-counting stage. This lets builtin tools reuse the main session's
/// routed/OAuth-aware provider without introducing a dependency cycle.
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

/// Value sent in the `anthropic-version` header on every request.
/// Matches the value in `api-client/src/anthropic.rs::ANTHROPIC_VERSION`.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Minimal Anthropic request builder for `POST /v1/messages`.
///
/// Replaces `AnthropicProvider` in the `BuiltinToolContext.provider`
/// field; only `build_request` is used by `WebSearchTool` — no retry, OAuth,
/// streaming, or telemetry. Ported 1:1 from `AnthropicProvider::build_request`
/// in lingxi-code/api-client/src/anthropic.rs.
///
/// Wire shape (byte-identical to the api-client origin):
/// - Method: POST
/// - URL: `{base_url}/v1/messages` (simple string concat, no trailing-slash
///   normalisation — mirrors api-client's `format!("{}/v1/messages", self.base_url)`)
/// - Headers: `x-api-key`, `anthropic-version: 2023-06-01`,
///   `content-type: application/json`, `accept: application/json`
/// - Body: `body.to_string()` (JSON serialised by caller)
/// - Timeout: 120 s (same as api-client's build_request)
///
/// The WebSearch tool attaches `anthropic-beta` and `user-agent` headers itself
/// after calling `build_request`, matching the upstream flow.
#[derive(Clone)]
pub struct AnthropicRequestBuilder {
    /// API key sent as `x-api-key`.
    pub api_key: String,
    /// Base URL (e.g. `https://api.anthropic.com`). No trailing slash expected;
    /// `/v1/messages` is appended with a literal `/`.
    pub base_url: String,
    /// Optional exact-token counter backed by the main session provider.
    mcp_token_counter: Option<Arc<dyn McpTokenCounter>>,
}

impl std::fmt::Debug for AnthropicRequestBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicRequestBuilder")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("has_mcp_token_counter", &self.mcp_token_counter.is_some())
            .finish()
    }
}

impl AnthropicRequestBuilder {
    /// Construct a new builder.
    ///
    /// Passing `None` for `base_url` uses the Anthropic default
    /// (`https://api.anthropic.com`).
    #[must_use]
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            mcp_token_counter: None,
        }
    }

    /// Attach the main session's routed exact-token counter.
    #[must_use]
    pub fn with_mcp_token_counter(mut self, counter: Arc<dyn McpTokenCounter>) -> Self {
        self.mcp_token_counter = Some(counter);
        self
    }

    /// Count transformed MCP result content through the session provider.
    pub async fn count_mcp_content_tokens(
        &self,
        model: &str,
        content: &serde_json::Value,
    ) -> Result<Option<u64>, String> {
        let Some(counter) = &self.mcp_token_counter else {
            return Ok(None);
        };
        counter.count_mcp_content_tokens(model, content).await
    }

    /// Build a non-streaming `POST /v1/messages` request.
    ///
    /// Replicates `AnthropicProvider::build_request` byte-for-byte:
    /// headers in the same order, same 120 s timeout. The caller owns the JSON
    /// body; this method only attaches headers and metadata.
    ///
    /// # Wire output (locked)
    /// ```text
    /// POST {base_url}/v1/messages
    /// x-api-key: <api_key>
    /// anthropic-version: 2023-06-01
    /// content-type: application/json
    /// accept: application/json
    /// <body as JSON string>
    /// ```
    #[must_use]
    pub fn build_request(&self, body: &serde_json::Value) -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Post,
            url: format!("{}/v1/messages", self.base_url),
            headers: vec![
                ("x-api-key".into(), self.api_key.clone()),
                ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
                ("content-type".into(), "application/json".into()),
                ("accept".into(), "application/json".into()),
            ],
            body: Some(body.to_string()),
            body_bytes: None,
            timeout: Some(std::time::Duration::from_secs(120)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::HttpMethod;
    use serde_json::json;

    struct FixedCounter;

    #[async_trait::async_trait]
    impl McpTokenCounter for FixedCounter {
        async fn count_mcp_content_tokens(
            &self,
            model: &str,
            content: &serde_json::Value,
        ) -> Result<Option<u64>, String> {
            assert_eq!(model, "claude-test");
            assert_eq!(content, &json!("payload"));
            Ok(Some(12_345))
        }
    }

    #[test]
    fn build_request_method_is_post() {
        let b = AnthropicRequestBuilder::new("key", None);
        let req = b.build_request(&json!({"model": "claude-3"}));
        assert_eq!(req.method, HttpMethod::Post);
    }

    #[test]
    fn build_request_url_joins_v1_messages() {
        let b = AnthropicRequestBuilder::new("key", Some("https://api.example.com".into()));
        let req = b.build_request(&json!({}));
        assert_eq!(req.url, "https://api.example.com/v1/messages");
    }

    #[test]
    fn build_request_url_default_base() {
        let b = AnthropicRequestBuilder::new("key", None);
        let req = b.build_request(&json!({}));
        assert_eq!(req.url, "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn build_request_headers_byte_identical_to_origin() {
        let b = AnthropicRequestBuilder::new("sk-test-abc", None);
        let req = b.build_request(&json!({"test": true}));
        // Exact header order and values as api-client's build_request.
        assert_eq!(req.headers[0], ("x-api-key".into(), "sk-test-abc".into()));
        assert_eq!(
            req.headers[1],
            ("anthropic-version".into(), "2023-06-01".into())
        );
        assert_eq!(
            req.headers[2],
            ("content-type".into(), "application/json".into())
        );
        assert_eq!(req.headers[3], ("accept".into(), "application/json".into()));
        assert_eq!(req.headers.len(), 4);
    }

    #[test]
    fn build_request_body_is_json_string() {
        let b = AnthropicRequestBuilder::new("key", None);
        let body_val = json!({"model": "x", "max_tokens": 4096});
        let req = b.build_request(&body_val);
        let body_str = req.body.expect("body present");
        let roundtrip: serde_json::Value = serde_json::from_str(&body_str).expect("valid JSON");
        assert_eq!(roundtrip, body_val);
    }

    #[test]
    fn build_request_timeout_is_120s() {
        let b = AnthropicRequestBuilder::new("key", None);
        let req = b.build_request(&json!({}));
        assert_eq!(req.timeout, Some(std::time::Duration::from_secs(120)));
    }

    #[tokio::test]
    async fn mcp_counter_is_optional_and_delegates_when_attached() {
        let plain = AnthropicRequestBuilder::new("secret", None);
        assert_eq!(
            plain
                .count_mcp_content_tokens("claude-test", &json!("payload"))
                .await
                .unwrap(),
            None
        );

        let wired = plain.with_mcp_token_counter(Arc::new(FixedCounter));
        assert_eq!(
            wired
                .count_mcp_content_tokens("claude-test", &json!("payload"))
                .await
                .unwrap(),
            Some(12_345)
        );
        let debug = format!("{wired:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("[REDACTED]"));
    }
}
