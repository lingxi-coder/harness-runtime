//! Host authentication: login UI and callbacks, secure credential storage,
//! credential selection, refresh coordination and background task lifecycle.
//!
//! `lingxi_llm_client::auth` owns provider authentication protocols, request
//! encoding, token response interpretation and networking. This module uses
//! those SDK operations and supplies application policy and platform services.

pub mod anthropic;
pub mod copilot;
pub mod external_aws;
pub mod lifecycle;
pub mod openai;
pub mod provider;
