//! OAuth flows folded in from the former `anthropic-oauth` / `openai-oauth`
//! crates. These implement `crate::CredentialProvider` over PKCE/device-code
//! refresh machinery; the auth abstractions already live in this crate.

pub mod anthropic;
pub mod openai;
