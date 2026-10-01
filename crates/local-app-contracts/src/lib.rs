//! `local-app-contracts` — the part of the Local App vocabulary that clients
//! share with the service without depending on it.
//!
//! The id grammar: the product's iOS and Android bridges and the task scope
//! check need to know whether a string is a well-formed app id, and none of
//! them should compile a service with bundled SQLite to find out. The crate
//! depends on nothing of the workspace, so the grammar cannot drift between
//! the service that mints ids and the code that validates them.
//!
//! The page bridge ([`bridge`]): the operations an app's page may request and
//! the shapes of the answers, which the service interprets and every host that
//! serves a page carries.

#![forbid(unsafe_code)]

pub mod approvals;
pub mod bridge;
pub mod diagnostics;
pub mod events;
pub mod guest_paths;

pub mod ids {
    //! Id grammars for local apps and the Host-issued handles.
    //!
    //! App ids must match `^[a-z0-9][a-z0-9-]{0,53}$` and are validated before
    //! ever being used in a path.

    /// Maximum app id length (regex `{0,53}` tail plus the leading character).
    pub const APP_ID_MAX_LEN: usize = 54;

    /// Host-issued authoring contract handle shape exposed by the Local App
    /// schemas. The random suffix is generated as lowercase hex, which provides
    /// 128 bits of entropy while remaining inside the public alphanumeric shape.
    pub const AUTHORING_HANDLE_PATTERN: &str = r"^contract_[A-Za-z0-9]{32}$";
    /// Host-issued QA handle shape exposed by the Local App schemas.
    pub const QA_HANDLE_PATTERN: &str = r"^qa_[A-Za-z0-9]{32}$";
    /// Prefix of a Host-issued authoring contract handle.
    pub const AUTHORING_HANDLE_PREFIX: &str = "contract_";
    /// Prefix of a Host-issued QA handle.
    pub const QA_HANDLE_PREFIX: &str = "qa_";
    /// Length of the alphanumeric suffix after a handle's prefix.
    pub const HANDLE_SUFFIX_LEN: usize = 32;

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

    #[cfg(test)]
    mod tests {
        use super::*;

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
        fn host_handles_have_exactly_the_advertised_shape() {
            let suffix = "a1B2c3D4".repeat(HANDLE_SUFFIX_LEN / 8);
            assert!(is_valid_authoring_handle(&format!(
                "{AUTHORING_HANDLE_PREFIX}{suffix}"
            )));
            assert!(is_valid_qa_handle(&format!("{QA_HANDLE_PREFIX}{suffix}")));
            // A handle of one kind is never a handle of the other.
            assert!(!is_valid_qa_handle(&format!(
                "{AUTHORING_HANDLE_PREFIX}{suffix}"
            )));
            assert!(!is_valid_authoring_handle(&format!(
                "{QA_HANDLE_PREFIX}{suffix}"
            )));
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
            }
        }
    }
}
