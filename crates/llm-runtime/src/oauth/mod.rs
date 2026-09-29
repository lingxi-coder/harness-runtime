//! Host OAuth orchestration: secure credential storage, login UI and callbacks,
//! refresh coordination, background task lifecycle and model retry policy.
//! Provider authentication protocols and networking are implemented in llm-client.

pub mod anthropic;
pub mod openai;

pub mod lifecycle;
