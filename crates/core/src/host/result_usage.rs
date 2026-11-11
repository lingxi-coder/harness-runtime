//! Usage accumulated between published SDK results, independent of session totals.

use crate::types::SessionId;

/// One provider-qualified model's changes since the last published result.
/// Signed counters retain late accounting corrections without duplicating a charge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultModelUsage {
    pub model: String,
    pub provider: String,
    pub nano_usd: i128,
    /// SDK costs accumulate each response's USD value in observation order.
    pub cost_usd: f64,
    pub input_tokens: i128,
    pub output_tokens: i128,
    pub reasoning_tokens: i128,
    pub cache_read_input_tokens: i128,
    pub cache_creation_input_tokens: i128,
    pub cache_creation_5m_input_tokens: i128,
    pub cache_creation_1h_input_tokens: i128,
    pub web_search_requests: i128,
}

/// Floating-point SDK projection alongside the authoritative integer charge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultCost {
    pub nano_usd: u64,
    pub cost_usd: f64,
}

/// A model's cumulative SDK cost; provider identity remains part of its key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultModelCost {
    pub model: String,
    pub provider: String,
    pub cost: ResultCost,
    /// Resume reconstructs model rows in a different native property order.
    pub restored: bool,
}

/// Cumulative cost projections survive consumption of the per-result interval.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultUsageTotals {
    pub total: Option<ResultCost>,
    pub models: Vec<ResultModelCost>,
}

/// Session accounting owner consumed synchronously at SDK result publication.
pub trait ResultUsageSource: Send + Sync {
    /// Start a query's usage view without consuming the SDK result interval.
    fn begin_query(&self, session_id: SessionId);

    /// Read the current query's observations before a result can be held.
    fn query_usage(&self, session_id: SessionId, read: &mut dyn FnMut(&[ResultModelUsage]));

    /// Publish this session's pending changes under the accounting lock. Reset
    /// only if the callback admits the frame to the outbound queue. Concurrent
    /// mutations belong wholly to this result or to the next one.
    fn publish_result_usage(
        &self,
        session_id: SessionId,
        publish: &mut dyn FnMut(&[ResultModelUsage], &ResultUsageTotals) -> bool,
    );
}
