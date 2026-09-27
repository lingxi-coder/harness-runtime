//! `tengu_uncompilable_ignore_pattern` event name + its `site` values.
//!
//! Emitted (CC 2.1.207 helper `oeg`) whenever a gitignore-style pattern fails
//! to compile at one of the ignore-consuming sites — the pattern is then
//! treated as matching nothing. The payload is `{ site }`, drawn from CC's
//! `neg` map: `claudemd_rule_globs`, `skill_paths`, `file_suggestions_ignore`,
//! `worktreeinclude`.
//!
//! These are NOT added to the count-locked `ALL_EVENT_NAMES` / `tengu_events.json`
//! fixture (that snapshot is from an OLDER claude event set; adding an entry
//! would break the byte-parity length lock). They live here for string-lock
//! testing only, mirroring [`crate::tengu::workflow`].

/// `tengu_uncompilable_ignore_pattern` — a gitignore-style pattern could not be
/// compiled and is treated as matching nothing.
pub const UNCOMPILABLE_IGNORE_PATTERN: &str = "tengu_uncompilable_ignore_pattern";

/// `site` value for the `.claudemd`/`.lingxi` conditional-rule glob site.
pub const SITE_CLAUDEMD_RULE_GLOBS: &str = "claudemd_rule_globs";
/// `site` value for the skill-path ignore site.
pub const SITE_SKILL_PATHS: &str = "skill_paths";
/// `site` value for the file-suggestions ignore site.
pub const SITE_FILE_SUGGESTIONS_IGNORE: &str = "file_suggestions_ignore";
/// `site` value for the `.worktreeinclude` copy site (`copyWorktreeIncludeFiles`).
pub const SITE_WORKTREEINCLUDE: &str = "worktreeinclude";

/// Every `site` value carried by `tengu_uncompilable_ignore_pattern`, in CC's
/// `neg`-map declaration order (string-lock only, NOT in ALL_EVENT_NAMES).
pub const SITES: &[&str] = &[
    SITE_CLAUDEMD_RULE_GLOBS,
    SITE_SKILL_PATHS,
    SITE_FILE_SUGGESTIONS_IGNORE,
    SITE_WORKTREEINCLUDE,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_name_is_byte_exact() {
        assert_eq!(
            UNCOMPILABLE_IGNORE_PATTERN,
            "tengu_uncompilable_ignore_pattern"
        );
    }

    #[test]
    fn site_values_are_byte_exact_in_neg_map_order() {
        assert_eq!(
            SITES,
            &[
                "claudemd_rule_globs",
                "skill_paths",
                "file_suggestions_ignore",
                "worktreeinclude",
            ]
        );
    }
}
