//! Id generation and validation for local apps.
//!
//! App ids follow the repo's feature-scoped persisted-id convention (see
//! `tools/cron::generate_cron_task_id`): random lowercase hex minted by the
//! store crate so every entry path (engine command, future FFI) produces the
//! SAME on-disk format. App ids must match `^[a-z0-9][a-z0-9-]{0,53}$` and are
//! validated before ever being used in a path.
//!
//! The grammar itself lives in `local-app-contracts`, which has no
//! dependencies, so clients can check an id without compiling the service;
//! this module re-exports it under its old path and adds what needs the
//! service: minting, and validators that answer with an [`AppError`].

use crate::error::AppError;
pub use local_app_contracts::ids::{
    is_valid_app_id, is_valid_authoring_handle, is_valid_qa_handle, APP_ID_MAX_LEN,
    AUTHORING_HANDLE_PATTERN, QA_HANDLE_PATTERN,
};
use local_app_contracts::ids::{AUTHORING_HANDLE_PREFIX, HANDLE_SUFFIX_LEN, QA_HANDLE_PREFIX};

fn random_hex(len: usize) -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"0123456789abcdef";
    let mut rng = rand::rng();
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        let idx = rng.random_range(0..ALPHABET.len());
        s.push(ALPHABET[idx] as char);
    }
    s
}

/// Mint a fresh eight-character lowercase-hex app id.
#[must_use]
pub fn generate_app_id() -> String {
    random_hex(8)
}

/// Mint a fresh interaction id (`int-` + 12 lowercase hex chars).
#[must_use]
pub fn generate_interaction_id() -> String {
    format!("int-{}", random_hex(12))
}

/// Mint a fresh suggestion id (`sugg-` + 12 lowercase hex chars).
#[must_use]
pub fn generate_suggestion_id() -> String {
    format!("sugg-{}", random_hex(12))
}

/// Mint a fresh Host-issued authoring contract handle.
#[must_use]
pub fn generate_authoring_handle() -> String {
    format!("{AUTHORING_HANDLE_PREFIX}{}", random_hex(HANDLE_SUFFIX_LEN))
}

/// Mint a fresh Host-issued QA handle.
#[must_use]
pub fn generate_qa_handle() -> String {
    format!("{QA_HANDLE_PREFIX}{}", random_hex(HANDLE_SUFFIX_LEN))
}

/// Validate a Host-issued authoring contract handle.
pub fn validate_authoring_handle(value: &str) -> Result<(), AppError> {
    if is_valid_authoring_handle(value) {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "invalid authoring handle {value:?}: must match {AUTHORING_HANDLE_PATTERN}"
        )))
    }
}

/// Validate a Host-issued QA handle.
pub fn validate_qa_handle(value: &str) -> Result<(), AppError> {
    if is_valid_qa_handle(value) {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "invalid QA handle {value:?}: must match {QA_HANDLE_PATTERN}"
        )))
    }
}

/// Validate `id` against the app-id grammar, rejecting anything that could
/// escape the apps root (path separators, `..`, absolute paths are all
/// impossible under the grammar).
pub fn validate_app_id(id: &str) -> Result<(), AppError> {
    if is_valid_app_id(id) {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "invalid app id {id:?}: must match ^[a-z0-9][a-z0-9-]{{0,53}}$"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppErrorCode;

    #[test]
    fn generated_app_ids_are_valid_and_hex() {
        for _ in 0..64 {
            let id = generate_app_id();
            assert_eq!(id.len(), 8);
            assert!(id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
            assert!(is_valid_app_id(&id));
        }
    }

    #[test]
    fn generated_interaction_and_suggestion_ids_are_prefixed() {
        assert!(generate_interaction_id().starts_with("int-"));
        assert!(generate_suggestion_id().starts_with("sugg-"));
        assert_eq!(generate_interaction_id().len(), 16);
        assert_eq!(generate_suggestion_id().len(), 17);
    }

    #[test]
    fn generated_host_handles_match_the_advertised_shape_and_are_random() {
        let authoring = generate_authoring_handle();
        let qa = generate_qa_handle();
        assert!(is_valid_authoring_handle(&authoring));
        assert!(is_valid_qa_handle(&qa));
        assert!(authoring["contract_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert!(qa["qa_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_ne!(authoring, generate_authoring_handle());
        assert_ne!(qa, generate_qa_handle());
        assert!(validate_authoring_handle(&authoring).is_ok());
        assert!(validate_qa_handle(&qa).is_ok());
    }

    #[test]
    fn rejects_invalid_and_traversal_ids() {
        let too_long = "a".repeat(55);
        for id in [
            "",
            "-leading-dash",
            "Upper",
            "under_score",
            "spa ce",
            "..",
            "../evil",
            "a/b",
            "a\\b",
            "a.b",
            "über",
            too_long.as_str(),
        ] {
            assert!(!is_valid_app_id(id), "expected invalid: {id}");
            let err = validate_app_id(id).unwrap_err();
            assert_eq!(err.code(), AppErrorCode::InvalidRequest);
        }
    }
}
