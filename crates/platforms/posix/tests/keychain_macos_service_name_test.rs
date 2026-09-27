//! Service-name helper tests — must match claude-code's
//! `macOsKeychainHelpers.ts` exactly.

use platform_posix::secure_storage::{compute_dir_hash, full_service_name};
use std::path::PathBuf;

#[test]
fn default_dir_yields_empty_hash_suffix() {
    let default_dir = PathBuf::from(format!(
        "{}/.lingxi",
        std::env::var("HOME").unwrap_or_else(|_| "/Users/test".into())
    ));
    let hash = compute_dir_hash(&default_dir, &default_dir);
    assert_eq!(hash, "", "default ~/.claude must produce empty dir_hash");
}

#[test]
fn non_default_dir_yields_8_char_hex() {
    let default = PathBuf::from("/Users/test/.lingxi");
    let custom = PathBuf::from("/Users/test/work/.claude-2");
    let hash = compute_dir_hash(&custom, &default);
    assert_eq!(hash.len(), 9, "expected `-` + 8 hex chars, got {hash:?}");
    assert!(hash.starts_with('-'));
    assert!(hash[1..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_lowercase() || c.is_ascii_digit())));
}

#[test]
fn service_name_default_oauth_layout() {
    let name = full_service_name("LingXi", "", "-credentials", "");
    assert_eq!(name, "LingXi-credentials");
}

#[test]
fn service_name_legacy_api_key_default() {
    let name = full_service_name("LingXi", "", "", "");
    assert_eq!(name, "LingXi");
}

#[test]
fn service_name_oauth_with_dir_hash() {
    let name = full_service_name("LingXi", "", "-credentials", "-abc12345");
    assert_eq!(name, "LingXi-credentials-abc12345");
}

#[test]
fn service_name_with_oauth_suffix_and_dir_hash() {
    let name = full_service_name("LingXi", "-staging", "-credentials", "-deadbeef");
    assert_eq!(name, "LingXi-staging-credentials-deadbeef");
}
