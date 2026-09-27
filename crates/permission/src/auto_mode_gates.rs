//! WIZARD-06 — pre-gather consent gates and availability markers (2.1.220).
//!
//! Several recon steps reach beyond the current repository: they call `gh`,
//! read shell history, walk the home directory for other checkouts, or mine
//! other projects' transcripts. Each is gated — on an explicit user answer
//! (Q1/Q2/Q3), on a policy gate, or on a capability being present — and when a
//! gate is closed the step renders one of the markers below INSTEAD of its
//! data.
//!
//! Two distinctions the wording is careful about, and which this module exists
//! to preserve byte-for-byte:
//!
//! * **Withheld vs. empty.** `NOT GATHERED` / `NOT WALKED` / "not queryable
//!   here" all mean *we did not or could not look*. They are deliberately
//!   different from the "found nothing" markers, because a proposal that reads
//!   an ungathered section as an empty one would draw conclusions from absent
//!   evidence. [`NO_REPOS_FOUND_WALK_CUT_SHORT`] is the sharpest case: it says
//!   outright to treat the result as unknown, not as none.
//!
//! * **Do not self-fetch.** Every consent-gated marker tells the model not to
//!   go get the data itself. That instruction is the enforcement point for the
//!   user's answer — without it the model could route around a declined gate by
//!   running its own search, `gh` call, or history read. A test below asserts
//!   each gated marker still carries it.
//!
//! [`HOME_REPOS_NETWORK_HOME`] is a gate of a different kind: a home directory
//! on a UNC share or automount cannot be touched without authenticating to, or
//! at least resolving, the named host, so the walk is skipped rather than
//! attempted and reported.

/// The org repo split was not collected: the user scoped to this project (Q2),
/// was not asked, or the policy gate is off.
pub const ORG_REPO_SPLIT_NOT_GATHERED: &str = r#"_NOT GATHERED — the user picked "just this project" (Q2), was not asked yet, or the policy gate is off. Do not fetch this yourself; infer the org posture from Repo facts and Q1 instead._"#;

/// Sibling org repo docs were not fetched (Q2 scope, or not yet asked).
pub const SIBLING_DOCS_NOT_GATHERED: &str = r#"_NOT GATHERED — the user picked "just this project" (Q2), or was not asked before this ran. No sibling repos were fetched. Do not fetch them yourself._"#;

/// Shell history was not read: the user did not opt in at Q3.
pub const SHELL_HISTORY_NOT_GATHERED: &str = r#"_NOT GATHERED — the user did not opt in at Q3, or was not asked before this ran. Treat shell history as "not queryable here". Do not read history files yourself._"#;

/// `U1d` — the home directory is a network path, so history was not read.
///
/// Distinct from the Q3 marker: nothing was declined here, the path itself is
/// one that cannot be touched without reaching a host.
pub const SHELL_HISTORY_NETWORK_HOME: &str = "_NOT GATHERED \u{2014} the home directory resolves to a network path. Treat shell history as \"not queryable here\". Do not read history files yourself._";

/// Shell history was not read: no home directory could be determined.
pub const SHELL_HISTORY_NO_HOME: &str = r#"_NOT GATHERED — no home directory could be determined. Treat shell history as "not queryable here". Do not read history files yourself._"#;

/// The home-directory repo walk did not run: no Q3 opt-in.
pub const HOME_REPOS_NOT_GATHERED: &str = r"_NOT GATHERED — the user did not opt in to looking beyond this repo (Q3), or was not asked before this ran. No home-directory contents were read. Do not run your own filesystem search to fill this in._";

/// The home directory is a network path. Touching one authenticates to, or
/// resolves, the named host, so the walk is skipped outright.
pub const HOME_REPOS_NETWORK_HOME: &str = r#"_NOT WALKED — the home directory resolves to a network path (UNC share or automount), and merely touching one authenticates to, or resolves, the named host. Treat other repos as "not queryable here"._"#;

/// The home directory could not be read, so no repos were walked.
pub const HOME_REPOS_UNREADABLE: &str = r#"_NOT WALKED — the home directory could not be read. Treat other repos as "not queryable here"._"#;

/// Other projects' transcripts were not mined: Q2 scope, not asked, or no
/// permission context was available to enforce `permissions.deny`.
pub const OTHER_PROJECT_TRANSCRIPTS_NOT_GATHERED: &str = r#"_NOT GATHERED — the user picked "just this project" (Q2), was not asked before this ran, or no permission context was available to enforce permissions.deny. No other project’s transcripts were read. Do not read them yourself; use only the per-project section above._"#;

/// Other projects' transcripts were not mined: no permission context, so
/// `permissions.deny` could not be enforced against them.
pub const OTHER_PROJECT_TRANSCRIPTS_NO_PERMISSION_CONTEXT: &str = r"_NOT GATHERED — no permission context was available to enforce permissions.deny, so no other project’s transcripts were read._";

/// The projects root was absent, unreadable, or enumeration timed out.
pub const OTHER_PROJECT_TRANSCRIPTS_UNAVAILABLE: &str = r"_Not queryable here — the projects root under the config home is absent or unreadable, or enumerating it exceeded the deadline. Treat other-project usage as unknown, not empty._";

/// A `gh`-backed lookup was skipped because nonessential traffic is disabled
/// or policy-restricted.
pub const NOT_QUERYABLE_NONESSENTIAL_TRAFFIC: &str =
    r"_Not queryable here (nonessential traffic disabled or policy-restricted)._";

/// As [`NOT_QUERYABLE_NONESSENTIAL_TRAFFIC`] but left open for a suffix (note
/// the trailing space and the missing closing `_`).
pub const NOT_QUERYABLE_NONESSENTIAL_TRAFFIC_PREFIX: &str =
    r"_Not queryable here (nonessential traffic disabled or policy-restricted). ";

/// `gh` was unavailable or unauthenticated.
pub const NOT_QUERYABLE_GH_UNAVAILABLE: &str =
    r"_Not queryable here (gh unavailable or unauthenticated)._";

/// The origin remote is not github.com.
pub const NOT_QUERYABLE_NOT_GITHUB: &str =
    r"_Not queryable here (origin remote is not github.com — GHE/other hosts not yet supported)._";

/// As [`NOT_QUERYABLE_NOT_GITHUB`] but left open for a suffix.
pub const NOT_QUERYABLE_NOT_GITHUB_PREFIX: &str =
    r"_Not queryable here (origin remote is not github.com — GHE/other hosts not yet supported). ";

/// The org/repo pair could not be derived from the origin remote. Left open
/// for a suffix.
pub const NOT_QUERYABLE_ORG_REPO_UNDERIVABLE_PREFIX: &str = r"_Not queryable here (org/repo not derivable from origin remote — missing, an unsupported or GHE host, or not a plain owner/repo URL shape). ";

/// The sibling-docs lookup ran and found nothing.
pub const NO_SIBLING_DOCS_FOUND: &str =
    r"_No sibling docs found (org repos have no LINGXI.md/README, or none listed)._";

/// The home walk ran to completion and found no other repos.
pub const NO_OTHER_REPOS_FOUND: &str = r"_No other git repos found under the home directory._";

/// The home walk was cut short before finding anything -- explicitly NOT the
/// same as finding none.
pub const NO_REPOS_FOUND_WALK_CUT_SHORT: &str =
    r"_No repos found before the walk was cut short — treat this as unknown, not as none._";

/// The home walk hit its time budget; the list is incomplete.
pub const WALK_HIT_TIME_BUDGET: &str = r"
_The walk hit its time budget — this list is INCOMPLETE, not exhaustive._";

/// The home walk hit its directory budget; the list is incomplete.
pub const WALK_HIT_DIRECTORY_BUDGET: &str = r"
_The walk hit its directory budget — this list is INCOMPLETE, not exhaustive._";

/// Command-word extraction hit its line cap or deadline.
pub const COMMAND_WORDS_INCOMPLETE: &str = r"
_Command-word extraction hit its line cap or deadline — the list below may be incomplete._";

/// The repo-wide bucket scan ended early; counts are a lower bound.
pub const BUCKET_SCAN_ENDED_EARLY: &str = r"
_The scan ended early (time/size budget or unreadable files) — counts are a lower bound and the list may be incomplete._";

/// Appended to the repo-facts section: explains that gh-backed lookups degrade
/// to a not-queryable marker while consent-gated parts render NOT GATHERED.
pub const REPO_FACTS_GH_EXPLAINER: &str = r#"
Repo visibility, rulesets/protected branches, and sibling org repo docs are gathered separately below via gh. Capability failures degrade to a "not queryable here" marker; the consent-gated parts (org repo split, sibling docs) render "NOT GATHERED" instead — do not fetch those yourself."#;

/// The consent-gated markers: every one stands in for data the user's answer
/// (or a policy gate) withheld, and every one must forbid self-fetching.
pub const CONSENT_GATED_MARKERS: [&str; 6] = [
    ORG_REPO_SPLIT_NOT_GATHERED,
    SIBLING_DOCS_NOT_GATHERED,
    SHELL_HISTORY_NOT_GATHERED,
    HOME_REPOS_NOT_GATHERED,
    OTHER_PROJECT_TRANSCRIPTS_NOT_GATHERED,
    OTHER_PROJECT_TRANSCRIPTS_NO_PERMISSION_CONTEXT,
];

/// Markers meaning "we could not look here", as opposed to "we looked and found
/// nothing". A proposal must not read these as empty evidence.
pub const UNAVAILABLE_MARKERS: [&str; 9] = [
    SHELL_HISTORY_NO_HOME,
    HOME_REPOS_NETWORK_HOME,
    HOME_REPOS_UNREADABLE,
    OTHER_PROJECT_TRANSCRIPTS_UNAVAILABLE,
    NOT_QUERYABLE_NONESSENTIAL_TRAFFIC,
    NOT_QUERYABLE_GH_UNAVAILABLE,
    NOT_QUERYABLE_NOT_GITHUB,
    NO_REPOS_FOUND_WALK_CUT_SHORT,
    BUCKET_SCAN_ENDED_EARLY,
];

/// Markers meaning a step ran cleanly and genuinely found nothing.
pub const EMPTY_RESULT_MARKERS: [&str; 2] = [NO_SIBLING_DOCS_FOUND, NO_OTHER_REPOS_FOUND];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_consent_gated_marker_forbids_self_fetching() {
        // This instruction is what actually enforces a declined gate: without it
        // the model could route around the user's answer with its own search,
        // `gh` call, or history read.
        for marker in CONSENT_GATED_MARKERS {
            assert!(
                marker.starts_with("_NOT GATHERED \u{2014} "),
                "gated marker must announce itself: {marker}"
            );
            let forbids = marker.contains("Do not fetch this yourself")
                || marker.contains("Do not fetch them yourself")
                || marker.contains("Do not read history files yourself")
                || marker.contains("Do not run your own filesystem search")
                || marker.contains("Do not read them yourself")
                || marker.contains("no other project\u{2019}s transcripts were read");
            assert!(forbids, "gated marker must forbid self-fetching: {marker}");
        }
    }

    #[test]
    fn withheld_is_never_spelled_the_same_as_empty() {
        // The whole point of the two vocabularies: a proposal that conflates
        // them draws conclusions from evidence that was never collected.
        for withheld in CONSENT_GATED_MARKERS
            .iter()
            .chain(UNAVAILABLE_MARKERS.iter())
        {
            for empty in EMPTY_RESULT_MARKERS {
                assert_ne!(*withheld, empty);
            }
        }
        assert!(NO_OTHER_REPOS_FOUND.contains("No other git repos found"));
        // ...and the cut-short case says outright not to read it as "none".
        assert!(NO_REPOS_FOUND_WALK_CUT_SHORT.contains("treat this as unknown, not as none"));
    }

    #[test]
    fn consent_gate_markers_are_byte_exact() {
        assert_eq!(
            ORG_REPO_SPLIT_NOT_GATHERED,
            "_NOT GATHERED \u{2014} the user picked \"just this project\" (Q2), was not asked yet, or the policy gate is off. Do not fetch this yourself; infer the org posture from Repo facts and Q1 instead._"
        );
        assert_eq!(
            SIBLING_DOCS_NOT_GATHERED,
            "_NOT GATHERED \u{2014} the user picked \"just this project\" (Q2), or was not asked before this ran. No sibling repos were fetched. Do not fetch them yourself._"
        );
        assert_eq!(
            SHELL_HISTORY_NOT_GATHERED,
            "_NOT GATHERED \u{2014} the user did not opt in at Q3, or was not asked before this ran. Treat shell history as \"not queryable here\". Do not read history files yourself._"
        );
        assert_eq!(
            HOME_REPOS_NOT_GATHERED,
            "_NOT GATHERED \u{2014} the user did not opt in to looking beyond this repo (Q3), or was not asked before this ran. No home-directory contents were read. Do not run your own filesystem search to fill this in._"
        );
    }

    #[test]
    fn availability_markers_are_byte_exact() {
        assert_eq!(
            NOT_QUERYABLE_NONESSENTIAL_TRAFFIC,
            "_Not queryable here (nonessential traffic disabled or policy-restricted)._"
        );
        assert_eq!(
            NOT_QUERYABLE_GH_UNAVAILABLE,
            "_Not queryable here (gh unavailable or unauthenticated)._"
        );
        assert_eq!(
            NO_SIBLING_DOCS_FOUND,
            "_No sibling docs found (org repos have no LINGXI.md/README, or none listed)._"
        );
        assert_eq!(
            NO_OTHER_REPOS_FOUND,
            "_No other git repos found under the home directory._"
        );
        assert_eq!(
            HOME_REPOS_UNREADABLE,
            "_NOT WALKED \u{2014} the home directory could not be read. Treat other repos as \"not queryable here\"._"
        );
    }

    #[test]
    fn network_home_is_skipped_rather_than_probed() {
        // Merely touching a UNC share or automount authenticates to / resolves
        // the named host, so this gate exists to avoid the side effect itself.
        assert!(HOME_REPOS_NETWORK_HOME.starts_with("_NOT WALKED \u{2014} "));
        assert!(HOME_REPOS_NETWORK_HOME
            .contains("merely touching one authenticates to, or resolves, the named host"));
    }

    #[test]
    fn open_ended_prefixes_keep_their_trailing_space_and_no_terminator() {
        // These variants are concatenated with a reason suffix, so they end with
        // a space and WITHOUT the closing markdown underscore.
        for prefix in [
            NOT_QUERYABLE_NONESSENTIAL_TRAFFIC_PREFIX,
            NOT_QUERYABLE_NOT_GITHUB_PREFIX,
            NOT_QUERYABLE_ORG_REPO_UNDERIVABLE_PREFIX,
        ] {
            assert!(prefix.ends_with(". "), "must stay open: {prefix}");
            assert!(!prefix.ends_with("._"));
        }
        // The closed forms are the same text, terminated.
        assert_eq!(
            format!("{}_", NOT_QUERYABLE_NONESSENTIAL_TRAFFIC_PREFIX.trim_end()),
            NOT_QUERYABLE_NONESSENTIAL_TRAFFIC
        );
        assert_eq!(
            format!("{}_", NOT_QUERYABLE_NOT_GITHUB_PREFIX.trim_end()),
            NOT_QUERYABLE_NOT_GITHUB
        );
    }

    #[test]
    fn incomplete_markers_flag_their_own_truncation() {
        for marker in [WALK_HIT_TIME_BUDGET, WALK_HIT_DIRECTORY_BUDGET] {
            assert!(marker.contains("INCOMPLETE, not exhaustive"));
        }
        assert!(COMMAND_WORDS_INCOMPLETE.contains("may be incomplete"));
        assert!(BUCKET_SCAN_ENDED_EARLY.contains("counts are a lower bound"));
    }

    #[test]
    fn repo_facts_explainer_names_both_degradation_paths() {
        assert!(REPO_FACTS_GH_EXPLAINER.contains("\"not queryable here\" marker"));
        assert!(REPO_FACTS_GH_EXPLAINER.contains("render \"NOT GATHERED\" instead"));
        assert!(REPO_FACTS_GH_EXPLAINER.contains("do not fetch those yourself"));
    }
}
