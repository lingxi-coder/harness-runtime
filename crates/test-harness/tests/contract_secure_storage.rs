//! Drive the platform [`SecureStorage`] impls through the canonical
//! contract suite.
//!
//! The macOS Keychain backend is gated on `LINGXI_TEST_KEYCHAIN=1` so the
//! suite is skipped on shared dev machines where popping `security`
//! authorisation prompts would be disruptive.

use tempfile::TempDir;
use test_harness::contracts::secure_storage::secure_storage_contract_tests;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn posix_plaintext_secure_storage_passes_contract() {
    let dir = TempDir::new().expect("tempdir");
    let s = platform_posix::PlainTextSecureStorage::new(dir.path().to_path_buf())
        .await
        .expect("plaintext storage init");
    secure_storage_contract_tests(&s).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_keychain_secure_storage_passes_contract() {
    if std::env::var("LINGXI_TEST_KEYCHAIN").ok().as_deref() != Some("1") {
        eprintln!("skipping macos_keychain_secure_storage: LINGXI_TEST_KEYCHAIN not set");
        return;
    }
    let dir = TempDir::new().expect("tempdir");
    let s = platform_posix::MacOsKeychainStorage::new(
        "lingxi-contract-test-user".into(),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        String::new(),
    )
    .expect("keychain storage init (security CLI must be on PATH)");
    secure_storage_contract_tests(&s).await;
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn windows_plaintext_secure_storage_passes_contract() {
    let dir = TempDir::new().expect("tempdir");
    let s = platform_windows::PlainTextSecureStorage::new(dir.path().to_path_buf())
        .await
        .expect("plaintext storage init");
    secure_storage_contract_tests(&s).await;
}
