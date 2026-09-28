//! Model capabilities exposed by the Harness SDK.
//!
//! Existing LLM policy and execution stay in the independent `llm-runtime`.
//! Decision contracts reserve a separate capability without registering a backend.

pub mod decision;
pub use llm_runtime as llm;
