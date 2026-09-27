//! WIZARD-06 — the interactive `/auto-mode-setup` wizard (2.1.220).
//!
//! Three questions run before any data is gathered, and their answers are
//! consent decisions, not preferences. Q2 and Q3 in particular authorise the
//! recon steps that reach outside the current repository:
//!
//! * **Q2 (scope)** — answering "all projects" authorises the `gh` lookup of
//!   sibling repositories in the user's GitHub org. The answer label discloses
//!   that ("also checks sibling repos in your GitHub org via gh") rather than
//!   burying it, because choosing it is what makes the network calls legitimate.
//! * **Q3 (depth)** — authorises reading shell history and/or other checkouts
//!   under the home directory. The prompt names both sources explicitly.
//!
//! Declining is enforced downstream by the markers in
//! [`crate::auto_mode_gates`]: a gate the user closed renders a `NOT GATHERED`
//! marker that also forbids the model from fetching the data itself. Q1
//! (posture) is the one genuinely preferential answer — it only steers phrasing
//! — and it reaches the model through the propose prompt's preamble.
//!
//! The answer VALUES (`personal`, `all`, `both`, …) are the wire vocabulary
//! shared with [`crate::auto_mode_propose`]'s prompt assembly and with the
//! `tengu_auto_mode_setup_wizard_answers` telemetry; the labels below are what
//! the user reads. Both are byte-exact.

// ── questions and answers ─────────────────────────────────────────────────────

/// Q1 asks how to characterise the user's work.
pub const Q1_POSTURE_PROMPT: &str = r"How would you describe the code you work on with Claude?";

/// Q1 answer: personal / hobby.
pub const Q1_POSTURE_PERSONAL: &str = r"Personal / hobby projects";

/// Q1 answer: open-source. The label spells out that pushes publish.
pub const Q1_POSTURE_OPEN_SOURCE: &str = r"Open-source (public repos — pushes publish)";

/// Q1 answer: work / enterprise.
pub const Q1_POSTURE_ENTERPRISE: &str = r"Work / enterprise (private repos, sensitive data)";

/// Q1 answer: mixed.
pub const Q1_POSTURE_MIXED: &str = r"Mixed — depends on the project";

/// Q2 asks whether the setup covers all projects or just this one.
pub const Q2_SCOPE_PROMPT: &str = r"Set this up for all your projects, or just this one?";

/// Q2 answer: all projects. The label discloses the gh org lookup this
/// enables -- the consent the org-repo-split and sibling-docs gathers rely on.
pub const Q2_SCOPE_ALL: &str =
    r"All projects (recommended — also checks sibling repos in your GitHub org via gh)";

/// Q2 answer: just this project.
pub const Q2_SCOPE_PROJECT: &str =
    r"Just this project (entries scoped to this repo's remotes and paths)";

/// Q3 asks whether to look beyond the repo. The prompt names both data
/// sources it would read: shell history and other home-directory checkouts.
pub const Q3_DEPTH_PROMPT: &str = r"Want me to look beyond this repo? Shell history (if your shell keeps one) and other checkouts in your home folder can help if you do a lot of work outside Claude.";

/// Q3 answer: read both shell history and other checkouts.
pub const Q3_DEPTH_BOTH: &str = r"Yes, both";

/// Q3 answer: shell history only.
pub const Q3_DEPTH_SHELL: &str = r"Just shell history";

/// Q3 answer: other checkouts only.
pub const Q3_DEPTH_REPOS: &str = r"Just other checkouts";

/// Q3 answer: nothing beyond this repo.
pub const Q3_DEPTH_HERE: &str = r"No, just here";

/// Asked first when the user already has auto-mode entries.
pub const EXISTING_ENTRIES_PROMPT: &str =
    r"You already have auto-mode entries — add to them, or start fresh?";

/// Keep the existing entries and add to them.
pub const EXISTING_ENTRIES_APPEND: &str = r"Add to them (keeps your existing entries)";

/// Replace the environment section wholesale.
pub const EXISTING_ENTRIES_REPLACE: &str = r"Start fresh (replaces the environment section)";

/// Abandon the wizard.
pub const EXISTING_ENTRIES_CANCEL: &str = r"Cancel";

// ── panel and status vocabulary ───────────────────────────────────────────────

/// Progress counter prefix (`Question `).
pub const QUESTION_COUNTER_PREFIX: &str = r"Question ";

/// Progress counter suffix (` of 3`).
pub const QUESTION_COUNTER_SUFFIX: &str = r" of 3";

/// The wizard panel title.
pub const WIZARD_TITLE: &str = r"Auto-mode setup";

/// Scan status when the gh org lookup is included.
pub const SCANNING_WITH_ORG: &str = r"Scanning your repo, recent sessions, and your GitHub org…";

/// Scan status without the org lookup.
pub const SCANNING_LOCAL_ONLY: &str = r"Scanning your repo and recent sessions…";

/// Second line of the scan status.
pub const SCANNING_SUFFIX: &str =
    r"then drafting a proposal — this can take a moment (Esc to cancel)";

/// Shown while the settings write is in flight.
pub const SAVING_STATUS: &str = r"Saving…";

/// Fallback error text.
pub const GENERIC_ERROR: &str = r"Something went wrong.";

/// Dismisses the panel.
pub const CLOSE_LABEL: &str = r"Close";

/// Shown when the scan moves to the background.
pub const BACKGROUND_START_NOTICE: &str =
    r"Gathering data and drafting your auto-mode setup; back soon";

/// Appended to the background notice when the org scan is included.
pub const BACKGROUND_START_ORG_SUFFIX: &str =
    r" (also scanning your GitHub org — stoppable from the background tasks list)";

/// Shown when the user declines the proposal.
pub const DISCARDED_NOTICE: &str =
    r"Discarded — nothing was saved. Re-run /auto-mode-setup anytime.";

/// Prefix of the unexpected-error notice.
pub const UNEXPECTED_ERROR_PREFIX: &str = r"Auto-mode setup hit an unexpected error and stopped: ";

/// Suffix of the unexpected-error notice.
pub const UNEXPECTED_ERROR_SUFFIX: &str = r". Re-run /auto-mode-setup to try again.";

/// Debug-log prefix for a background crash.
pub const BACKGROUND_CRASH_PREFIX: &str = r"background auto-mode setup crashed: ";

/// Title used for a background crash notification.
pub const BACKGROUND_CRASH_TITLE: &str = r"background auto-mode setup crashed";

/// Rejects arguments to the interactive entry point and names the
/// non-interactive flags instead.
pub const TAKES_NO_ARGUMENTS: &str = r"/auto-mode-setup doesn’t take arguments — run it on its own and answer the questions. In non-interactive mode, use --propose / --apply-file.";

/// A setup is already scanning.
pub const ALREADY_IN_PROGRESS: &str = r"An auto-mode setup is already in progress — the proposal review will pop up when the scan finishes. (The scan shows in the background tasks list.)";

/// A setup is already finishing.
pub const ALREADY_WRAPPING_UP: &str = r"An auto-mode setup is already wrapping up — if its proposal review hasn’t popped up, answer it when it does; if you just stopped the scan, it’s winding down — try again in a moment.";

// ── answer values (wire vocabulary) ──────────────────────────────────────────

/// Q1 answer values, paired with their labels.
pub const POSTURE_VALUES: [(&str, &str); 4] = [
    ("personal", Q1_POSTURE_PERSONAL),
    ("open-source", Q1_POSTURE_OPEN_SOURCE),
    ("enterprise", Q1_POSTURE_ENTERPRISE),
    ("mixed", Q1_POSTURE_MIXED),
];

/// Q2 answer values, paired with their labels.
pub const SCOPE_VALUES: [(&str, &str); 2] = [("all", Q2_SCOPE_ALL), ("project", Q2_SCOPE_PROJECT)];

/// Q3 answer values, paired with their labels.
pub const DEPTH_VALUES: [(&str, &str); 4] = [
    ("both", Q3_DEPTH_BOTH),
    ("shell", Q3_DEPTH_SHELL),
    ("repos", Q3_DEPTH_REPOS),
    ("here", Q3_DEPTH_HERE),
];

/// The `tengu_auto_mode_setup_wizard_answers` field names.
pub const ANSWER_FIELDS: [&str; 3] = ["posture", "scope", "depth"];

/// The `tengu_auto_mode_setup_wizard_shown` field recording whether the user
/// already had auto-mode entries.
pub const FIELD_HAS_EXISTING: &str = "has_existing";

/// The `tengu_auto_mode_setup_wizard_resolved` `choice` values.
pub const RESOLVED_CHOICES: [&str; 6] = ["none", "cancel", "done", "saved", "error", "decline"];

/// The review-step telemetry event.
pub const AUTO_MODE_SETUP_REVIEW_EVENT: &str = "auto_mode_setup_review";
/// The wizard-level telemetry event (crash / error reporting).
pub const AUTO_MODE_SETUP_WIZARD_EVENT: &str = "auto_mode_setup_wizard";
/// Recorded when the background setup task crashes.
pub const WIZARD_CODE_BACKGROUND_CRASH: &str = "background_crash";

/// The review outcomes.
pub const REVIEW_OUTCOMES: [&str; 3] = ["accept", "decline", "cancelled"];

/// Whether the Q2 answer authorises the `gh` org lookups (sibling docs and the
/// org repo split).
#[must_use]
pub fn scope_authorises_org_lookup(scope: &str) -> bool {
    scope == "all"
}

/// Whether the Q3 answer authorises reading shell history.
#[must_use]
pub fn depth_authorises_shell_history(depth: &str) -> bool {
    matches!(depth, "both" | "shell")
}

/// Whether the Q3 answer authorises walking the home directory for other
/// checkouts.
#[must_use]
pub fn depth_authorises_home_repos(depth: &str) -> bool {
    matches!(depth, "both" | "repos")
}

/// `Question {n} of 3`.
#[must_use]
pub fn question_counter(n: usize) -> String {
    format!("{QUESTION_COUNTER_PREFIX}{n}{QUESTION_COUNTER_SUFFIX}")
}

/// The unexpected-error notice: prefix + error + suffix.
#[must_use]
pub fn unexpected_error_message(err: &str) -> String {
    format!("{UNEXPECTED_ERROR_PREFIX}{err}{UNEXPECTED_ERROR_SUFFIX}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_and_answers_are_byte_exact() {
        assert_eq!(
            Q1_POSTURE_PROMPT,
            "How would you describe the code you work on with Claude?"
        );
        assert_eq!(Q1_POSTURE_PERSONAL, "Personal / hobby projects");
        assert_eq!(
            Q1_POSTURE_OPEN_SOURCE,
            "Open-source (public repos \u{2014} pushes publish)"
        );
        assert_eq!(
            Q1_POSTURE_ENTERPRISE,
            "Work / enterprise (private repos, sensitive data)"
        );
        assert_eq!(Q1_POSTURE_MIXED, "Mixed \u{2014} depends on the project");

        assert_eq!(
            Q2_SCOPE_PROMPT,
            "Set this up for all your projects, or just this one?"
        );
        assert_eq!(
            Q2_SCOPE_PROJECT,
            "Just this project (entries scoped to this repo's remotes and paths)"
        );

        assert_eq!(Q3_DEPTH_BOTH, "Yes, both");
        assert_eq!(Q3_DEPTH_SHELL, "Just shell history");
        assert_eq!(Q3_DEPTH_REPOS, "Just other checkouts");
        assert_eq!(Q3_DEPTH_HERE, "No, just here");
    }

    #[test]
    fn consent_questions_disclose_what_they_authorise() {
        // Q2's "all projects" answer is what makes the gh org calls legitimate,
        // so the label has to say the calls happen.
        assert_eq!(
            Q2_SCOPE_ALL,
            "All projects (recommended \u{2014} also checks sibling repos in your GitHub org via gh)"
        );
        assert!(Q2_SCOPE_ALL.contains("your GitHub org via gh"));

        // Q3 must name both data sources it would read.
        assert_eq!(
            Q3_DEPTH_PROMPT,
            "Want me to look beyond this repo? Shell history (if your shell keeps one) and other checkouts in your home folder can help if you do a lot of work outside Claude."
        );
        assert!(Q3_DEPTH_PROMPT.contains("Shell history"));
        assert!(Q3_DEPTH_PROMPT.contains("other checkouts in your home folder"));
    }

    #[test]
    fn answer_values_map_to_the_reaches_they_authorise() {
        assert!(scope_authorises_org_lookup("all"));
        assert!(!scope_authorises_org_lookup("project"));

        assert!(depth_authorises_shell_history("both"));
        assert!(depth_authorises_shell_history("shell"));
        assert!(!depth_authorises_shell_history("repos"));
        assert!(!depth_authorises_shell_history("here"));

        assert!(depth_authorises_home_repos("both"));
        assert!(depth_authorises_home_repos("repos"));
        assert!(!depth_authorises_home_repos("shell"));
        assert!(!depth_authorises_home_repos("here"));
    }

    #[test]
    fn an_unrecognised_answer_authorises_nothing() {
        // Fail closed: a value we do not recognise must never widen the reach.
        for unknown in ["", "yes", "ALL", "Both", "unknown"] {
            assert!(!scope_authorises_org_lookup(unknown));
            assert!(!depth_authorises_shell_history(unknown));
            assert!(!depth_authorises_home_repos(unknown));
        }
    }

    #[test]
    fn answer_value_tables_are_complete_and_unique() {
        assert_eq!(POSTURE_VALUES.len(), 4);
        assert_eq!(SCOPE_VALUES.len(), 2);
        assert_eq!(DEPTH_VALUES.len(), 4);
        for table in [&POSTURE_VALUES[..], &SCOPE_VALUES[..], &DEPTH_VALUES[..]] {
            let mut values: Vec<&str> = table.iter().map(|(v, _)| *v).collect();
            values.sort_unstable();
            let before = values.len();
            values.dedup();
            assert_eq!(values.len(), before, "duplicate answer value");
            for (_, label) in table {
                assert!(!label.is_empty());
            }
        }
        // Every depth value except "here" authorises at least one extra reach.
        for (value, _) in DEPTH_VALUES {
            let reaches =
                depth_authorises_shell_history(value) || depth_authorises_home_repos(value);
            assert_eq!(reaches, value != "here");
        }
    }

    #[test]
    fn existing_entries_question_is_byte_exact() {
        assert_eq!(
            EXISTING_ENTRIES_PROMPT,
            "You already have auto-mode entries \u{2014} add to them, or start fresh?"
        );
        assert_eq!(
            EXISTING_ENTRIES_APPEND,
            "Add to them (keeps your existing entries)"
        );
        assert_eq!(
            EXISTING_ENTRIES_REPLACE,
            "Start fresh (replaces the environment section)"
        );
        assert_eq!(EXISTING_ENTRIES_CANCEL, "Cancel");
    }

    #[test]
    fn telemetry_vocabulary_is_byte_exact() {
        assert_eq!(ANSWER_FIELDS, ["posture", "scope", "depth"]);
        assert_eq!(FIELD_HAS_EXISTING, "has_existing");
        assert_eq!(
            RESOLVED_CHOICES,
            ["none", "cancel", "done", "saved", "error", "decline"]
        );
        assert_eq!(AUTO_MODE_SETUP_REVIEW_EVENT, "auto_mode_setup_review");
        assert_eq!(AUTO_MODE_SETUP_WIZARD_EVENT, "auto_mode_setup_wizard");
        assert_eq!(WIZARD_CODE_BACKGROUND_CRASH, "background_crash");
        assert_eq!(REVIEW_OUTCOMES, ["accept", "decline", "cancelled"]);
    }

    #[test]
    fn panel_vocabulary_is_byte_exact() {
        assert_eq!(question_counter(2), "Question 2 of 3");
        assert_eq!(WIZARD_TITLE, "Auto-mode setup");
        assert_eq!(
            SCANNING_WITH_ORG,
            "Scanning your repo, recent sessions, and your GitHub org\u{2026}"
        );
        assert_eq!(
            SCANNING_LOCAL_ONLY,
            "Scanning your repo and recent sessions\u{2026}"
        );
        assert_eq!(SAVING_STATUS, "Saving\u{2026}");
        assert_eq!(GENERIC_ERROR, "Something went wrong.");
        assert_eq!(CLOSE_LABEL, "Close");
        assert_eq!(
            DISCARDED_NOTICE,
            "Discarded \u{2014} nothing was saved. Re-run /auto-mode-setup anytime."
        );
        assert_eq!(
            unexpected_error_message("EPIPE"),
            "Auto-mode setup hit an unexpected error and stopped: EPIPE. Re-run /auto-mode-setup to try again."
        );
    }

    #[test]
    fn scan_status_only_mentions_the_org_when_it_was_authorised() {
        // The two variants exist so the status never claims a reach the user
        // did not authorise at Q2.
        assert!(SCANNING_WITH_ORG.contains("your GitHub org"));
        assert!(!SCANNING_LOCAL_ONLY.contains("org"));
        assert!(BACKGROUND_START_ORG_SUFFIX.contains("also scanning your GitHub org"));
        assert!(!BACKGROUND_START_NOTICE.contains("org"));
    }

    #[test]
    fn interactive_entry_point_rejects_arguments_and_names_the_flags() {
        assert_eq!(
            TAKES_NO_ARGUMENTS,
            "/auto-mode-setup doesn\u{2019}t take arguments \u{2014} run it on its own and answer the questions. In non-interactive mode, use --propose / --apply-file."
        );
        assert!(TAKES_NO_ARGUMENTS.contains("--propose / --apply-file"));
    }

    #[test]
    fn reentry_messages_distinguish_scanning_from_wrapping_up() {
        assert!(ALREADY_IN_PROGRESS.contains("already in progress"));
        assert!(ALREADY_IN_PROGRESS.contains("background tasks list"));
        assert!(ALREADY_WRAPPING_UP.contains("already wrapping up"));
        assert_ne!(ALREADY_IN_PROGRESS, ALREADY_WRAPPING_UP);
    }
}
