//! Prompt-too-long (413 / "prompt is too long") detection + token-gap parsing.
//!
//! Canonical implementation has moved to `llm_runtime::model::prompt_too_long`.
//! This module re-exports all public items so that existing `compaction::prompt_too_long`
//! consumers continue to work without changes.

pub use llm_runtime::model::prompt_too_long::{
    is_prompt_too_long_body, parse_prompt_too_long_token_counts, prompt_too_long_token_gap,
    PROMPT_TOO_LONG_ERROR_MESSAGE,
};
