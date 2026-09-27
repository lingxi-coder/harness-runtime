//! Id generation and validation for local apps.
//!
//! App ids follow the repo's feature-scoped persisted-id convention (see
//! `tools/cron::generate_cron_task_id`): random lowercase hex minted by the
//! store crate so every entry path (engine command, future FFI) produces the
//! SAME on-disk format. App ids must match `^[a-z0-9][a-z0-9-]{0,53}$` and are
//! validated before ever being used in a path.

use crate::error::AppError;

/// Maximum app id length (regex `{0,53}` tail plus the leading character).
pub const APP_ID_MAX_LEN: usize = 54;

/// Host-issued authoring contract handle shape exposed by the Local App
/// schemas. The random suffix is generated as lowercase hex, which provides
/// 128 bits of entropy while remaining inside the public alphanumeric shape.
pub const AUTHORING_HANDLE_PATTERN: &str = r"^contract_[A-Za-z0-9]{32}$";
/// Host-issued QA handle shape exposed by the Local App schemas.
pub const QA_HANDLE_PATTERN: &str = r"^qa_[A-Za-z0-9]{32}$";
const AUTHORING_HANDLE_PREFIX: &str = "contract_";
const QA_HANDLE_PREFIX: &str = "qa_";
const HANDLE_SUFFIX_LEN: usize = 32;

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

fn is_valid_host_handle(value: &str, prefix: &str) -> bool {
    value.len() == prefix.len() + HANDLE_SUFFIX_LEN
        && value.starts_with(prefix)
        && value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
}

/// True iff `value` has the Host-issued authoring handle shape.
#[must_use]
pub fn is_valid_authoring_handle(value: &str) -> bool {
    is_valid_host_handle(value, AUTHORING_HANDLE_PREFIX)
}

/// True iff `value` has the Host-issued QA handle shape.
#[must_use]
pub fn is_valid_qa_handle(value: &str) -> bool {
    is_valid_host_handle(value, QA_HANDLE_PREFIX)
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

/// True iff `id` matches `^[a-z0-9][a-z0-9-]{0,53}$`.
#[must_use]
pub fn is_valid_app_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > APP_ID_MAX_LEN {
        return false;
    }
    let first_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    first_ok
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
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
    fn host_handle_validators_reject_counters_and_wrong_prefixes() {
        for value in [
            "contract_1",
            "contract_0000000000000000000000000000000",
            "authoring_00000000000000000000000000000000",
            "qa_1",
            "qa_0000000000000000000000000000000",
            "contract_0000000000000000000000000000000/",
        ] {
            assert!(!is_valid_authoring_handle(value));
            assert!(!is_valid_qa_handle(value));
        }
    }

    #[test]
    fn accepts_valid_ids() {
        let max_len = "a".repeat(54);
        for id in ["a", "0", "abc-123", "9-", max_len.as_str()] {
            assert!(is_valid_app_id(id), "expected valid: {id}");
        }
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
