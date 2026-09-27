//! Moved to llm-runtime; re-exported for in-orchestrator callers.
pub use llm_runtime::model::{
    betas, count_tokens, fallback, overflow, prompt_too_long, rate_limit, retry, telemetry,
    thinking, user_agent,
};
