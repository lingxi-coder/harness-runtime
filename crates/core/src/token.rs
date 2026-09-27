//! Token accounting. Pricing/budget logic lives in `lingxi-cost` (Plan 2).

use serde::{Deserialize, Serialize};

/// Token usage reported by the API for a single response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed for this request.
    pub input_tokens: u64,
    /// Output tokens generated for this response.
    pub output_tokens: u64,
    /// Input tokens that were written into the prompt cache.
    pub cache_creation_input_tokens: u64,
    /// Input tokens that were served from the prompt cache.
    pub cache_read_input_tokens: u64,
}

impl Usage {
    /// Accumulate another `Usage` into this one in place.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    /// Sum of all token fields.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }
}
