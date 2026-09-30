//! Secure storage backends for desktop hosts.
//!
//! [`PlainTextSecureStorage`] (fallback), [`MacOsKeychainStorage`] (macOS, via
//! the signed LingXi credential broker) and `LinuxSecretStorage` (Linux, via the `libsecret`
//! `secret-tool` CLI) implement [`lingxi_core::host::SecureStorage`]. The
//! [`secure_storage_for_platform`] helper picks the best backend per OS,
//! with a documented plaintext-fallback warning when the preferred backend
//! cannot initialise.

pub mod factory;
pub mod helpers;
pub mod linux;
pub mod macos;
pub mod plaintext;

pub use factory::{
    plaintext_secure_storage, secure_storage_for_platform, secure_storage_for_policy,
};
pub use helpers::{
    compute_dir_hash, full_service_name, CREDENTIALS_SERVICE_SUFFIX, KEYCHAIN_CACHE_TTL,
    SECURITY_STDIN_LINE_LIMIT,
};
pub use linux::LinuxSecretStorage;
pub use macos::MacOsKeychainStorage;
pub use plaintext::PlainTextSecureStorage;
