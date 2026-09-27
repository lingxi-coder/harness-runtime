//! Secret subsystem: secret-kind labels, a gitleaks-subset scanner, a
//! boundary-aware redaction policy, a cached Anthropic credential manager,
//! and a macOS keychain prefetch.
//!
//! See design spec §6 (Secrets) and the M1 plan for the canonical scope.
#![forbid(unsafe_code)]
pub mod credential;
pub mod keychain_prefetch;
pub mod kinds;
pub mod redaction;
pub mod scanner;

pub use credential::{masked_credential_preview, CredentialError, CredentialManager, OAuthTokens};
pub use keychain_prefetch::KeychainPrefetch;
pub use kinds::SecretKind;
pub use redaction::{BoundaryPolicy, RedactionBoundary, RedactionOutcome, RedactionPolicy};
pub use scanner::{SecretDetection, SecretScanner};
