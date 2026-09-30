//! `OpenAI` / ChatGPT-account OAuth login for llm-runtime.
//!
//! Mirrors the `anthropic-oauth` crate: PKCE + device-code login, token
//! refresh, and a `crate::CredentialProvider` that serves a
//! `Credential::ChatGptOAuth` (bearer + ChatGPT-Account-ID). Byte-aligned with
//! codex's `ChatGPT` auth (see docs/superpowers/specs/2026-06-16-p2-chatgpt-oauth-login-design.md).

#![forbid(unsafe_code)]

pub mod callback;
pub mod credential_provider;
pub mod device_code;
pub mod external_tokens;
pub mod handle;
pub mod login;
pub mod pat;
pub mod refresh;

#[cfg(test)]
pub(crate) mod testsupport;

pub use credential_provider::OpenAiOAuthCredentialProvider;
pub use device_code::run_device_code_login;
pub use external_tokens::ExternalTokensCredentialProvider;
pub use handle::{BrowserOpener, OpenAiAuthError, OpenAiLoginInfo, OpenAiOAuthHandle};
pub use login::{init_refresh_driver, OAuthError};
pub use pat::PatCredentialProvider;
pub use refresh::{AuthState, RefreshDriver};
