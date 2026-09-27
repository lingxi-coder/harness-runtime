//! [`SecureStorage`] contract.
//!
//! Verifies the round-trip semantics every `SecureStorage` impl must honour:
//!
//! * `is_encrypted()` answers a `bool` without panicking.
//! * `store` followed by `retrieve` yields byte-identical payload bytes.
//! * `delete` removes the entry such that the next `retrieve` returns `None`.
//! * `retrieve` on a never-stored `(service, account)` pair returns
//!   `Ok(None)` — *not* an error.
//!
//! The macOS Keychain backend's argv/stdin shape, the dir-hash service-name
//! discriminator, and the 30 s cache TTL all belong to platform-local tests
//! (`platforms/posix/src/secure_storage/`); this contract sits at the trait
//! surface only.

use platform_api::SecureStorage;
use protocol::{SecretKindDto, SecureStorageData, SecureStorageMetadata};

const SERVICE: &str = "lingxi-contract-test";
const ACCOUNT: &str = "contract@test.local";

/// Run the standard [`SecureStorage`] contract against an impl.
///
/// # Panics
///
/// Panics on the first invariant violation.
pub async fn secure_storage_contract_tests<S: SecureStorage>(s: &S) {
    // Clean any leftover entry from a previous run before exercising the
    // contract so we don't accidentally pass on stale state.
    let _ = s.delete(SERVICE, ACCOUNT).await;
    test_is_encrypted_returns_bool(s);
    test_store_then_retrieve_roundtrips(s).await;
    test_delete_removes_entry(s).await;
    test_retrieve_missing_returns_none(s).await;
    // Best-effort cleanup so concurrent contract runs (or backends backed by
    // OS-wide stores like the macOS Keychain) don't leak state between
    // test invocations.
    let _ = s.delete(SERVICE, ACCOUNT).await;
}

fn test_is_encrypted_returns_bool<S: SecureStorage>(s: &S) {
    let _ = s.is_encrypted();
}

async fn test_store_then_retrieve_roundtrips<S: SecureStorage>(s: &S) {
    let payload: &[u8] = b"contract-payload-bytes";
    let data = sample_data(payload);
    s.store(SERVICE, ACCOUNT, data)
        .await
        .expect("store must succeed");
    let got = s
        .retrieve(SERVICE, ACCOUNT)
        .await
        .expect("retrieve must succeed")
        .expect("retrieve must yield Some after store");
    assert_eq!(
        got.expose_secret_bytes(),
        payload,
        "retrieved bytes must equal stored bytes"
    );
}

async fn test_delete_removes_entry<S: SecureStorage>(s: &S) {
    s.store(SERVICE, ACCOUNT, sample_data(b"x"))
        .await
        .expect("store");
    s.delete(SERVICE, ACCOUNT).await.expect("delete");
    let got = s.retrieve(SERVICE, ACCOUNT).await.expect("retrieve");
    assert!(got.is_none(), "after delete, retrieve must return None");
}

async fn test_retrieve_missing_returns_none<S: SecureStorage>(s: &S) {
    let got = s
        .retrieve(SERVICE, "never-stored-account@contract-test")
        .await
        .expect("retrieve of a missing key must succeed (returning None)");
    assert!(got.is_none(), "missing entry must yield None, not error");
}

/// Build a fresh [`SecureStorageData`] holding `bytes`. Timestamps use
/// `UNIX_EPOCH` so the metadata is deterministic across runs.
fn sample_data(bytes: &[u8]) -> SecureStorageData {
    SecureStorageData::new(
        bytes.to_vec(),
        SecureStorageMetadata {
            created_at: std::time::UNIX_EPOCH,
            last_accessed: None,
            kind: SecretKindDto("contract_test".to_string()),
        },
    )
}
