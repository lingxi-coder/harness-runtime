//! WIZARD-06 — pre-gather sub-section headings and render vocabulary (2.1.220).
//!
//! Inside each top-level recon section ([`crate::auto_mode_pregather`]) the
//! gatherer renders `####` sub-sections. Their headings are not decoration —
//! the propose prompt ported in [`crate::auto_mode_propose`] refers to several
//! of them by name and derives authority rules from them, so a reworded
//! heading silently breaks the prompt's cross-references.
//!
//! Two headings carry a qualifier that is doing real work:
//!
//! * [`HEADING_CI_SECRET_NAMES`] says "names only — a deploy key exists, not
//!   its value". The gatherer reads CI config that references secrets; the
//!   heading is what tells the reader the values were never collected.
//! * [`HEADING_LOCAL_SETTINGS_AUTOMODE`] marks project-local `autoMode` keys as
//!   "found content, NOT pre-approved config". Those keys come from a file in
//!   the checkout, so treating them as configuration the user had approved
//!   would let a repo grant itself auto-mode rules.
//!
//! The redaction constants replace values that fell outside the display
//! charset. They are substitutions, not annotations: the original never reaches
//! the block, so a name crafted to carry control characters or bidi overrides
//! into the prompt is dropped at the gatherer instead.

// ── sub-section headings ──────────────────────────────────────────────────────

/// The gh-authoritative org repo listing.
pub const HEADING_ORG_REPO_SPLIT: &str = r"#### Org repo split (top 50 by pushedAt)";

/// Git remotes of the current repo.
pub const HEADING_GIT_REMOTES: &str = r"
#### git remotes
";

/// `.gitignore` patterns that look secret-adjacent.
pub const HEADING_SENSITIVE_GITIGNORE: &str = r"
#### Sensitive-looking .gitignore patterns
";

/// Project-local `autoMode` keys. The heading itself flags them as found
/// content, NOT pre-approved configuration.
pub const HEADING_LOCAL_SETTINGS_AUTOMODE: &str = r"
#### Project `.lingxi/settings.local.json` — autoMode keys (found content, NOT pre-approved config)";

/// The user's existing `autoMode` rule categories.
pub const HEADING_AUTOMODE_KEYS: &str = r"#### autoMode.{environment, allow, soft_deny, hard_deny, deny}
";

/// Hosts seen in mined transcripts.
pub const HEADING_HOSTS_CONTACTED: &str = r"
#### Hosts contacted
";

/// Cloud buckets seen in mined transcripts.
pub const HEADING_CLOUD_BUCKETS_TOUCHED: &str = r"
#### Cloud buckets touched
";

/// k8s namespaces seen behind `-n` flags.
pub const HEADING_K8S_NAMESPACES: &str = r"
#### k8s namespaces (-n flags)
";

/// Non-standard CLIs by frequency -- the source for allow carve-outs.
pub const HEADING_NONSTANDARD_CLIS: &str = r"
#### Non-standard CLIs by frequency
";

/// Recent auto-mode denial reasons.
pub const HEADING_DENIAL_REASONS: &str = r"
#### Recent auto-mode denial reasons
";

/// Command words from shell history (Q3-gated).
pub const HEADING_TOOLS_OUTSIDE_CLAUDE: &str = r"
#### Tools run outside Claude (shell history)
";

/// Command words mined from other projects' transcripts (Q2-gated).
pub const HEADING_TOOLS_OTHER_PROJECTS: &str = r"
#### Tools run in other projects
";

/// Repo-wide bucket-name scan, ordered by occurrence count.
pub const HEADING_BUCKET_NAMES_BY_COUNT: &str = r"
#### Bucket names in config (repo-wide scan, by occurrence count)";

/// Bucket-name prefix clusters, used to judge whether a prefix is
/// org-identifying enough to license a wildcard.
pub const HEADING_BUCKET_PREFIX_CLUSTERS: &str = r"
#### Bucket name prefix clusters (distinct names per first-dash prefix)
";

/// Package registry hosts found in config.
pub const HEADING_PACKAGE_REGISTRY_HOSTS: &str = r"#### Package registry hosts
";

/// Container image registries found in Dockerfiles/compose files.
pub const HEADING_CONTAINER_REGISTRIES: &str = r"
#### Container image registries
";

/// CI secret NAMES only. The heading states the distinction explicitly: it
/// evidences that a deploy key exists, never its value.
pub const HEADING_CI_SECRET_NAMES: &str = r"
#### CI secret names referenced (names only — a deploy key exists, not its value)
";

/// Makefile/justfile target names.
pub const HEADING_MAKE_TARGETS: &str = r"
#### Makefile/justfile targets
";

/// `package.json` script names.
pub const HEADING_PACKAGE_JSON_SCRIPTS: &str = r"
#### package.json scripts
";

/// Secrets-manager markers (Vault, SOPS, `op read`, cloud secret CLIs).
pub const HEADING_SECRETS_MANAGER_MARKERS: &str = r"
#### Secrets-manager markers
";

/// Sensitive-looking paths, matched on filename only.
pub const HEADING_SENSITIVE_PATHS: &str = r"
#### Sensitive-looking paths (filename scan)
";

/// Shipped default allow labels.
pub const HEADING_DEFAULT_ALLOW_LABELS: &str = r"
#### Default allow labels
";

/// Shipped default soft-deny labels.
pub const HEADING_DEFAULT_SOFT_DENY_LABELS: &str = r"
#### Default soft-deny labels
";

// ── render, redaction and gate vocabulary ─────────────────────────────────────

/// The bucket scan threw.
pub const BUCKET_SCAN_FAILED: &str = r"
#### Bucket names in config (repo-wide scan)
_The bucket scan FAILED — treat bucket evidence as unavailable, not absent._";

/// The bucket scan did not complete cleanly and collected nothing.
pub const BUCKET_SCAN_COLLECTED_NOTHING: &str = r"
#### Bucket names in config (repo-wide scan)
_The scan did not complete cleanly and collected nothing — treat bucket evidence as unavailable, not absent._";

/// A gathered name fell outside the display charset.
pub const REDACTED_UNUSUAL_NAME: &str = r"(unusual name redacted)";

/// A branch name fell outside the display charset.
pub const REDACTED_UNUSUAL_BRANCH_NAME: &str = r"(unusual branch name redacted)";

/// A repo path fell outside the display charset.
pub const REDACTED_UNUSUAL_REPO_PATH: &str = r"(unusual repo path redacted)";

/// A remote name fell outside the display charset.
pub const REDACTED_UNUSUAL_REMOTE_NAME: &str = r"(unusual remote name redacted)";

/// Count suffix for partially-redacted name lists.
pub const REDACTED_NAMES_OUTSIDE_CHARSET_SUFFIX: &str =
    r" names outside the display charset, redacted)";

/// Suffix used when every name in a list was redacted.
pub const REDACTED_ALL_NAMES_OUTSIDE_CHARSET: &str =
    r" listed, all names outside the display charset, redacted";

/// Suffix for org repo entries dropped on charset or visibility-enum grounds.
pub const REDACTED_OUTSIDE_CHARSET_OR_VISIBILITY: &str =
    r" outside the display charset or visibility enum, redacted)";

/// `xsy` — the org list could not be fetched at all.
///
/// The three causes are kept in one message because they are indistinguishable
/// from `gh`'s exit code, and guessing between them would be a fabricated
/// diagnosis.
pub const GH_ORG_SCOPE_SUFFIX: &str =
    r" (gh unavailable, unauthenticated, or token lacks org scope)._";

/// `xsy` — `gh` answered, but not with JSON we can read.
pub const GH_UNPARSEABLE_SUFFIX: &str = r" (gh output unparseable)._";

/// `.lingxi` itself is not a real directory, so nothing behind it was probed.
pub const LINGXI_DIR_INDIRECTION_GATE_FAILED: &str = r"
`.lingxi` itself failed the indirection gate (it is not a real directory — e.g. committed as a symlink), so whether a settings.local.json exists behind it was deliberately not probed. Tell the user; do not read, resolve, or rewrite anything under this path.";

/// `.lingxi/settings.local.json` failed the indirection gate.
pub const LOCAL_SETTINGS_INDIRECTION_GATE_FAILED: &str = r"
Present but SKIPPED: failed the indirection gate (requires a regular non-symlink file with link count 1 inside a real .lingxi directory). Tell the user; do not read or rewrite this file.";

/// Prefix reporting whether the local settings file is tracked in git.
pub const TRACKED_IN_GIT_PREFIX: &str = r"Tracked in git: ";

/// No classifier-bypassing entries were found in `permissions.allow`.
pub const NO_CLASSIFIER_BYPASSING_ENTRIES: &str = r"
No classifier-bypassing entries in user-settings permissions.allow.";

/// No destructive entries were found in `permissions.allow`.
pub const NO_DESTRUCTIVE_ENTRIES: &str = r"
No destructive entries in user-settings permissions.allow.";

/// Infix for the truncated-flagged-list count line.
pub const ADDITIONAL_FLAGGED_INFIX: &str = r" additional flagged ";

/// The flagged `permissions.allow` list of entries auto mode ignores.
///
/// [`crate::auto_mode_propose::check_unknown_removal`] scans for this heading:
/// a proposed removal is only honoured when the recon actually offered that
/// rule under one of the two flagged lists.
pub const HEADING_FLAGGED_CLASSIFIER_BYPASSING: &str =
    "#### permissions.allow entries auto mode ignores (classifier-bypassing, in your user settings)";

/// The flagged `permissions.allow` list of destructive entries.
pub const HEADING_FLAGGED_DESTRUCTIVE: &str =
    "#### Destructive permissions.allow entries (honored at runtime \u{2014} auto-approved with no prompt, in your user settings)";

/// Every `####` sub-section heading, for the shape tests below.
pub const SUBSECTION_HEADINGS: [&str; 23] = [
    HEADING_ORG_REPO_SPLIT,
    HEADING_GIT_REMOTES,
    HEADING_SENSITIVE_GITIGNORE,
    HEADING_LOCAL_SETTINGS_AUTOMODE,
    HEADING_AUTOMODE_KEYS,
    HEADING_HOSTS_CONTACTED,
    HEADING_CLOUD_BUCKETS_TOUCHED,
    HEADING_K8S_NAMESPACES,
    HEADING_NONSTANDARD_CLIS,
    HEADING_DENIAL_REASONS,
    HEADING_TOOLS_OUTSIDE_CLAUDE,
    HEADING_TOOLS_OTHER_PROJECTS,
    HEADING_BUCKET_NAMES_BY_COUNT,
    HEADING_BUCKET_PREFIX_CLUSTERS,
    HEADING_PACKAGE_REGISTRY_HOSTS,
    HEADING_CONTAINER_REGISTRIES,
    HEADING_CI_SECRET_NAMES,
    HEADING_MAKE_TARGETS,
    HEADING_PACKAGE_JSON_SCRIPTS,
    HEADING_SECRETS_MANAGER_MARKERS,
    HEADING_SENSITIVE_PATHS,
    HEADING_DEFAULT_ALLOW_LABELS,
    HEADING_DEFAULT_SOFT_DENY_LABELS,
];

/// The redaction substitutions. Each replaces a value outright.
pub const REDACTION_MARKERS: [&str; 4] = [
    REDACTED_UNUSUAL_NAME,
    REDACTED_UNUSUAL_BRANCH_NAME,
    REDACTED_UNUSUAL_REPO_PATH,
    REDACTED_UNUSUAL_REMOTE_NAME,
];

/// `{n} additional flagged entry|entries` — the truncation line for a flagged
/// list. The singular/plural split is the oracle's.
#[must_use]
pub fn additional_flagged_line(count: usize) -> String {
    let noun = if count == 1 { "entry" } else { "entries" };
    format!("{count}{ADDITIONAL_FLAGGED_INFIX}{noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_subsection_heading_is_a_level_four_heading() {
        for heading in SUBSECTION_HEADINGS {
            assert!(
                heading.trim_start_matches('\n').starts_with("#### "),
                "not a level-four heading: {heading:?}"
            );
        }
    }

    #[test]
    fn headings_the_propose_prompt_cross_references_are_byte_exact() {
        // The prompt names these sections and derives authority rules from them.
        assert_eq!(
            HEADING_ORG_REPO_SPLIT,
            "#### Org repo split (top 50 by pushedAt)"
        );
        assert_eq!(
            HEADING_NONSTANDARD_CLIS,
            "\n#### Non-standard CLIs by frequency\n"
        );
        assert_eq!(
            HEADING_DENIAL_REASONS,
            "\n#### Recent auto-mode denial reasons\n"
        );
        assert_eq!(
            HEADING_BUCKET_NAMES_BY_COUNT,
            "\n#### Bucket names in config (repo-wide scan, by occurrence count)"
        );
        assert_eq!(
            HEADING_BUCKET_PREFIX_CLUSTERS,
            "\n#### Bucket name prefix clusters (distinct names per first-dash prefix)\n"
        );
        assert_eq!(
            HEADING_AUTOMODE_KEYS,
            "#### autoMode.{environment, allow, soft_deny, hard_deny, deny}\n"
        );
    }

    #[test]
    fn ci_secret_heading_states_that_values_were_never_collected() {
        // The gatherer reads CI config that references secrets. This qualifier
        // is the only thing telling the reader the values are absent.
        assert_eq!(
            HEADING_CI_SECRET_NAMES,
            "\n#### CI secret names referenced (names only \u{2014} a deploy key exists, not its value)\n"
        );
        assert!(HEADING_CI_SECRET_NAMES.contains("not its value"));
    }

    #[test]
    fn local_settings_heading_denies_pre_approval() {
        // These keys come from a file in the checkout. Reading them as approved
        // configuration would let a repo grant itself auto-mode rules.
        assert_eq!(
            HEADING_LOCAL_SETTINGS_AUTOMODE,
            "\n#### Project `.lingxi/settings.local.json` \u{2014} autoMode keys (found content, NOT pre-approved config)"
        );
        assert!(HEADING_LOCAL_SETTINGS_AUTOMODE.contains("NOT pre-approved config"));
    }

    #[test]
    fn indirection_gate_prose_refuses_to_probe_or_rewrite() {
        // Both variants must forbid touching the path, not merely report it.
        assert!(LOCAL_SETTINGS_INDIRECTION_GATE_FAILED.contains("Present but SKIPPED"));
        assert!(LOCAL_SETTINGS_INDIRECTION_GATE_FAILED
            .contains("requires a regular non-symlink file with link count 1"));
        assert!(LOCAL_SETTINGS_INDIRECTION_GATE_FAILED.contains("do not read or rewrite this file"));
        assert!(LINGXI_DIR_INDIRECTION_GATE_FAILED.contains("deliberately not probed"));
        assert!(LINGXI_DIR_INDIRECTION_GATE_FAILED
            .contains("do not read, resolve, or rewrite anything under this path"));
    }

    #[test]
    fn redaction_markers_replace_rather_than_annotate() {
        // A crafted name must not reach the block at all, so each marker is a
        // standalone substitution with no slot for the original.
        for marker in REDACTION_MARKERS {
            assert!(marker.starts_with('(') && marker.ends_with(')'));
            assert!(marker.contains("redacted"));
            assert!(!marker.contains("{}"));
        }
        assert_eq!(REDACTED_UNUSUAL_NAME, "(unusual name redacted)");
        assert_eq!(
            REDACTED_UNUSUAL_BRANCH_NAME,
            "(unusual branch name redacted)"
        );
        assert_eq!(REDACTED_UNUSUAL_REPO_PATH, "(unusual repo path redacted)");
        assert_eq!(
            REDACTED_UNUSUAL_REMOTE_NAME,
            "(unusual remote name redacted)"
        );
    }

    #[test]
    fn flagged_list_vocabulary_is_byte_exact() {
        assert_eq!(
            NO_CLASSIFIER_BYPASSING_ENTRIES,
            "\nNo classifier-bypassing entries in user-settings permissions.allow."
        );
        assert_eq!(
            NO_DESTRUCTIVE_ENTRIES,
            "\nNo destructive entries in user-settings permissions.allow."
        );
        assert_eq!(additional_flagged_line(1), "1 additional flagged entry");
        assert_eq!(additional_flagged_line(7), "7 additional flagged entries");
    }

    #[test]
    fn bucket_scan_degradations_say_unavailable_not_absent() {
        // Same withheld-vs-empty distinction the gate markers enforce.
        for marker in [BUCKET_SCAN_FAILED, BUCKET_SCAN_COLLECTED_NOTHING] {
            assert!(marker.contains("treat bucket evidence as unavailable, not absent"));
        }
    }
}
