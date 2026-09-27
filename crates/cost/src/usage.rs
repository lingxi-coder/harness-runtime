//! Per-turn and per-session usage records: token counts by class, server-side
//! tool counters, and an optional API speed selector.
//!
//! Values are pure counters — converting to money happens in
//! [`crate::calculator::CostCalculator`].

use crate::pricing::TokenClass;
use serde::{Deserialize, Serialize};

/// Token counters bucketed by [`TokenClass`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input (prompt) tokens.
    pub input: u64,
    /// Output (completion) tokens.
    pub output: u64,
    /// Tokens written into the prompt cache (standard 5-minute TTL).
    pub cache_write: u64,
    /// Tokens read from the prompt cache.
    pub cache_read: u64,
    /// Reasoning / thinking output tokens.
    pub reasoning_output: u64,
    /// Tokens written into the ephemeral 1-hour prompt cache.
    ///
    /// Mirrors `cache_creation.ephemeral_1h_input_tokens` in the Anthropic API
    /// response (binary field `promptCacheWrite1hTokens`).
    #[serde(default)]
    pub cache_write_1h: u64,
}

/// Composite usage record: tokens plus optional non-token counters and
/// optional speed hint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Per-class token counts.
    pub tokens: TokenUsage,
    /// Optional server-side tool counters (web search, etc.).
    pub server_tool_use: Option<ServerToolUsage>,
    /// Optional API speed selector.
    pub speed: Option<ApiSpeed>,
}

/// Counters for server-side tools that bill per-request rather than per-token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerToolUsage {
    /// Number of server-side web search requests.
    pub web_search_requests: u32,
}

/// Hint about which API speed tier was used for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiSpeed {
    /// Standard-latency tier.
    Standard,
    /// Fast (priority / low-latency) tier.
    Fast,
}

impl Usage {
    /// Return the token count for the given [`TokenClass`].
    #[must_use]
    pub fn tokens_for(&self, class: TokenClass) -> u64 {
        match class {
            TokenClass::Input => self.tokens.input,
            TokenClass::Output => self.tokens.output,
            TokenClass::CacheWrite => self.tokens.cache_write,
            TokenClass::CacheRead => self.tokens.cache_read,
            TokenClass::ReasoningOutput => self.tokens.reasoning_output,
            TokenClass::CacheWrite1h => self.tokens.cache_write_1h,
        }
    }

    /// Total context-window tokens at the time of an API call.
    ///
    /// Mirrors `getTokenCountFromUsage` (`utils/tokens.ts:46-66`):
    /// `input + cache_creation + cache_read + output`. This is the full context
    /// size reported by the last API response and is the input the compaction
    /// threshold math reads. `reasoning_output` is excluded to match TS, which
    /// sums only `input_tokens + cache_creation_input_tokens +
    /// cache_read_input_tokens + output_tokens` (reasoning is already folded
    /// into `output_tokens` on the wire).
    #[must_use]
    pub fn total_context_tokens(&self) -> u64 {
        self.tokens
            .input
            .saturating_add(self.tokens.cache_write)
            .saturating_add(self.tokens.cache_read)
            .saturating_add(self.tokens.output)
    }

    /// Sum of all token classes.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.tokens
            .input
            .saturating_add(self.tokens.output)
            .saturating_add(self.tokens.cache_write)
            .saturating_add(self.tokens.cache_read)
            .saturating_add(self.tokens.reasoning_output)
            .saturating_add(self.tokens.cache_write_1h)
    }

    /// Accumulate `other` into `self`, merging token counts and server-tool
    /// counters component-wise.
    pub fn add(&mut self, other: &Usage) {
        self.tokens.input = self.tokens.input.saturating_add(other.tokens.input);
        self.tokens.output = self.tokens.output.saturating_add(other.tokens.output);
        self.tokens.cache_write = self
            .tokens
            .cache_write
            .saturating_add(other.tokens.cache_write);
        self.tokens.cache_read = self
            .tokens
            .cache_read
            .saturating_add(other.tokens.cache_read);
        self.tokens.reasoning_output = self
            .tokens
            .reasoning_output
            .saturating_add(other.tokens.reasoning_output);
        self.tokens.cache_write_1h = self
            .tokens
            .cache_write_1h
            .saturating_add(other.tokens.cache_write_1h);
        if let Some(s) = other.server_tool_use {
            let dst = self
                .server_tool_use
                .get_or_insert(ServerToolUsage::default());
            dst.web_search_requests = dst
                .web_search_requests
                .saturating_add(s.web_search_requests);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_context_tokens_sums_input_cache_and_output() {
        let usage = Usage {
            tokens: TokenUsage {
                input: 100,
                output: 40,
                cache_write: 10,
                cache_read: 25,
                // reasoning_output is excluded from total_context_tokens (it is
                // folded into output_tokens on the wire — mirrors TS).
                reasoning_output: 7,
                cache_write_1h: 0,
            },
            ..Usage::default()
        };
        // input + cache_write + cache_read + output = 100 + 10 + 25 + 40 = 175.
        assert_eq!(usage.total_context_tokens(), 175);
        // total_tokens includes reasoning_output but not cache_write_1h (0 here).
        assert_eq!(usage.total_tokens(), 182);
    }

    #[test]
    fn total_context_tokens_zero_for_default() {
        assert_eq!(Usage::default().total_context_tokens(), 0);
    }
}
