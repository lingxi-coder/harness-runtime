//! `OpenAI` / ChatGPT-account OAuth login for llm-runtime.
//!
//! Mirrors the `anthropic-oauth` crate: PKCE + device-code login, token
//! refresh, and a `crate::CredentialProvider` that serves a
//! `Credential::ChatGptOAuth` (bearer + ChatGPT-Account-ID). Byte-aligned with
//! codex's `ChatGPT` auth (see docs/superpowers/specs/2026-06-16-p2-chatgpt-oauth-login-design.md).

#![forbid(unsafe_code)]

pub mod callback;
pub mod client;
pub mod credential_provider;
pub mod device_code;
pub mod external_tokens;
pub mod handle;
pub mod pat;
pub mod refresh;

#[cfg(test)]
pub(crate) mod testsupport;

pub use client::{init_refresh_driver, OAuthError, OpenAiOAuthClient};
pub use credential_provider::OpenAiOAuthCredentialProvider;
pub use device_code::{request_device_code, run_device_code_login, DeviceUserCode, PollOutcome};
pub use external_tokens::ExternalTokensCredentialProvider;
pub use handle::{BrowserOpener, OpenAiAuthError, OpenAiLoginInfo, OpenAiOAuthHandle};
pub use lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig;
pub use pat::{whoami, PatCredentialProvider, PatMetadata};
pub use refresh::{AuthState, RefreshDriver};
