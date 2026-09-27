//! WIZARD-06 — per-section fact rendering vocabulary (2.1.220).
//!
//! The remaining pre-gather strings: the labels each fact section renders, the
//! cap/deadline notices that admit a scan stopped early, the provenance notes,
//! and the flagged-`permissions.allow` vocabulary. Together with
//! [`crate::auto_mode_pregather`], [`crate::auto_mode_gates`] and
//! [`crate::auto_mode_sections`] this completes the block's text.
//!
//! Several of these carry a claim the rest of the system depends on:
//!
//! * [`CLASSIFY_ALL_SHELL_NOTE`] — when `classifyAllShell` is on, auto mode
//!   ignores *every* Bash and PowerShell allow rule at runtime. That is a
//!   superset of the entries the flagged lists show, so without this note a
//!   user could read a short flagged list as "these few rules are the problem"
//!   when in fact none of their shell allow rules apply. It also says the rules
//!   still apply OUTSIDE auto mode, so removing them is not a no-op.
//! * [`FLAGGED_ENTRY_UNRENDERABLE`] — an entry that cannot be safely rendered
//!   is reported for manual review rather than silently dropped. A rule that
//!   vanished from the list would look like a rule that was not there.
//! * [`REPOS_FOUND_HEADER`] — records that userinfo and any path beyond
//!   `owner/repo` are stripped when a remote is parsed, so credentials embedded
//!   in a remote URL never reach the block.
//! * [`SHELL_HISTORY_PROVENANCE_NOTE`] / [`OTHER_PROJECTS_PROVENANCE_NOTE`] —
//!   only extracted command WORDS entered the transcript, never raw history or
//!   command lines; the shell note additionally warns that the files themselves
//!   carry inline secrets.
//!
//! The cap and deadline notices exist for the same reason as the gate markers:
//! a truncated scan must not read as a complete one.

/// Appended to the per-project usage section: says other projects are a
/// separate, Q2-gated opt-in.
pub const PROJECT_USAGE_SCOPE_NOTE: &str = r"
Other projects’ transcripts are NOT mined here (a Q2 opt-in). Shell history and other checkouts under ~ have their own sections below.";

/// Provenance note for other-project mining: raw command LINES never entered
/// the transcript, only the extracted command words.
pub const OTHER_PROJECTS_PROVENANCE_NOTE: &str = r"
The user opted into this at Q2. Raw command lines were never read into the transcript — only the command words above. Merge these with the per-project counts in the section above.";

/// Provenance note for shell history. It also warns the reader off the files
/// themselves, which "carry inline secrets".
pub const SHELL_HISTORY_PROVENANCE_NOTE: &str = r"
The user opted into this at Q3. Raw history lines were never read into the transcript — only the command words above. Do not read these files yourself; they carry inline secrets.";

/// Home-walk results are candidates only, to be kept solely when their org is
/// already corroborated elsewhere.
pub const HOME_REPOS_CANDIDATE_NOTE: &str = r"
These are CANDIDATES, not vetted context: keep only the ones whose org already appears in Repo facts or the sibling-docs section.";

/// The project has no transcripts to mine.
pub const NO_TRANSCRIPT_HISTORY: &str = r"_no transcript history for this project_";

/// Count line prefix.
pub const TRANSCRIPTS_SCANNED_PREFIX: &str = r"Transcripts scanned: ";

/// Count line infix (other-projects form).
pub const ENUMERATED_BASH_COMMANDS_INFIX: &str = r" enumerated); Bash commands seen: ";

/// Count line infix (per-project form).
pub const BASH_COMMANDS_SEEN_INFIX: &str = r"; Bash commands seen: ";

/// Count line infix for the selected/enumerated split.
pub const SELECTED_FROM_INFIX: &str = r" selected (from ";

/// Count line infix naming the enumeration subset.
pub const FIRST_ENUMERATED_OF_INFIX: &str = r" first-enumerated of ";

/// Count line suffix for oversized transcripts.
pub const SKIPPED_AS_OVERSIZED_SUFFIX: &str = r" skipped as oversized)";

/// Warns that the recency ranking is drawn only from the enumerated subset.
pub const TRANSCRIPT_SELECTION_CAVEAT: &str = r" transcripts were considered; the most-recent selection is drawn from that subset, so a recent session in a project past the cap may be missing._";

/// Partial project coverage -- explicitly "unknown, not empty".
pub const PROJECTS_PARTIAL_COVERAGE: &str = r" could not be enumerated (unreadable, transient error, or past the enumeration cap) — coverage is partial; treat missing projects as unknown, not empty._";

/// Transcripts vanished or were refused as symlink/hardlink aliases.
pub const TRANSCRIPTS_UNREADABLE: &str =
    r" could not be read (removed mid-gather, or refused as a symlink/hardlink alias)._";

/// Transcripts skipped by permissions.deny, an untrusted network path, or a
/// resolution outside the projects directory.
pub const TRANSCRIPTS_DENY_SKIPPED: &str = r" not read — a permissions.deny rule covers the path, it is an untrusted network path, or it resolved outside the projects directory._";

/// Prefix listing paths the read-deny gate withheld.
pub const READ_DENY_GATE_PREFIX: &str = r"
_Skipped by the read-deny gate: ";

/// Tail of the aggregate/deadline notices.
pub const NOT_SCANNED_SUFFIX: &str = r" not scanned._";

/// The aggregate byte cap stopped the scan.
pub const AGGREGATE_BYTE_CAP_PREFIX: &str = r"
_Aggregate byte cap reached (";

/// Infix of the aggregate-cap notice.
pub const AGGREGATE_BYTE_CAP_INFIX: &str = r" MiB) — remaining ";

/// Only the most recent part of each oversized file was scanned.
pub const PER_FILE_CAP_SUFFIX: &str =
    r" MiB per-file cap — only the most recent part of each was scanned._";

/// The gather deadline stopped the scan.
pub const DEADLINE_REACHED_PREFIX: &str = r"
_Deadline reached — remaining ";

/// The enumeration cap stopped the scan.
pub const ENUMERATION_CAP_PREFIX: &str = r"
_Enumeration cap reached — the ";

/// A rendered value was truncated.
pub const TRUNCATED_AT_PREFIX: &str = r"
…[truncated at ";

/// Enumeration stop reason.
pub const ENUMERATION_DEADLINE_REACHED: &str = r"enumeration deadline reached";

/// Config scan stop reason.
pub const CONFIG_READ_TIMED_OUT: &str = r"config read timed out";

/// One of the shell-history files considered.
pub const FISH_HISTORY_FILE: &str = r"fish_history";

/// Prefix of the shell-history status line.
pub const STATUS_PREFIX: &str = r"Status: ";
/// Separator between the status word and the file count.
pub const STATUS_SEPARATOR: &str = " \u{2014} ";

/// Shell-history status infix.
pub const FILES_READ_INFIX: &str = r" file(s) read: ";

/// Warns that with `classifyAllShell` on, auto mode ignores EVERY Bash and
/// PowerShell allow rule at runtime -- a superset of what the flagged lists
/// show -- while those rules still apply outside auto mode.
pub const CLASSIFY_ALL_SHELL_NOTE: &str = r"
_Note: classifyAllShell is active, so at runtime auto mode ignores every Bash/PowerShell allow rule — a superset of the entries flagged here, including any shell entries in the destructive list; outside auto mode all of these rules still apply._";

/// A flagged entry could not be rendered or auto-removed, so the user is told
/// to review `permissions.allow` by hand rather than it being silently dropped.
pub const FLAGGED_ENTRY_UNRENDERABLE: &str = r" can't be shown or auto-removed (unusual characters or length) — the user should review permissions.allow by hand.";

/// The flagged list was capped; re-running surfaces the rest.
pub const FLAGGED_LIST_CAPPED: &str = r" more flagged entries not shown (list capped) — re-run /auto-mode-setup after this cleanup to see the rest";

/// Telemetry event for the flagged-allow review.
pub const FLAGGED_ALLOW_EVENT: &str = r"auto_mode_flagged_allow";

/// No settings file exists.
pub const NO_SETTINGS_FILE: &str = r"(no settings file)";

/// The settings file exists but could not be read.
pub const SETTINGS_PRESENT_BUT_UNREADABLE: &str = r"settings file present but unreadable";

/// Prefix for a validation failure of the existing block.
pub const AUTOMODE_BLOCK_FAILED_VALIDATION_PREFIX: &str = r"autoMode block failed validation: ";

/// The on-disk `autoMode` is an array, which setup refuses to rewrite.
pub const EXISTING_AUTOMODE_IS_ARRAY: &str = r"the existing autoMode value in the settings file is an array — remove or fix it, then re-run setup.";

/// Suffix for a skipped project-local settings file.
pub const LOCAL_SETTINGS_SKIPPED_SUFFIX: &str =
    r" — skipped. Tell the user; do not read or rewrite this file.";

/// The local settings file is tracked in git, so it is repo-authored.
pub const TRACKED_IN_GIT_YES: &str = r"yes — repo-authored";

/// Untracked -- and the wording refuses the inverse inference.
pub const TRACKED_IN_GIT_NO: &str = r"no — but untracked does not prove user-authored";

/// Repo-facts label.
pub const REPO_DEFAULT_BRANCH_PREFIX: &str = r"Default branch: ";

/// Repo-facts label.
pub const REPO_POSTURE_SIGNALS_PREFIX: &str = r"Posture signals present: ";

/// Repo-facts label.
pub const REPO_TRACKED_FILE_COUNT_PREFIX: &str = r"Tracked file count: ";

/// The repo has no remotes configured.
pub const NO_REMOTES: &str = r"(no remotes)";

/// `origin/HEAD` is unset.
pub const UNKNOWN_DEFAULT_BRANCH: &str = r"(unknown — origin/HEAD unset)";

/// Remote list truncation suffix.
pub const REMOTE_LINES_OMITTED_SUFFIX: &str = r" more remote lines omitted]";

/// A discovered repo has no remote.
pub const NO_REMOTE_CONFIGURED: &str = r"(no remote configured)";

/// A discovered repo's remote host is unrecognised, so it is withheld.
pub const REMOTE_NOT_KNOWN_HOST: &str = r"(remote not on a known VCS host; not shown)";

/// The gitdir escapes the home directory, so remotes are not read.
pub const GITDIR_OUTSIDE_HOME: &str =
    r"(gitdir points outside the home directory — remotes not read)";

/// Header for the home-walk results. It records the redaction contract:
/// userinfo and any path beyond `owner/repo` are stripped when the remote is
/// parsed, so credentials embedded in a remote URL never reach the block.
pub const REPOS_FOUND_HEADER: &str = r"Repos found (path — `host/org/repo` remotes; userinfo and any path beyond owner/repo are stripped at the parse):
";

/// Label for the README excerpt.
pub const DOC_README_HEAD_LABEL: &str = r"./README.md (head)";

/// Label for the CONTRIBUTING excerpt.
pub const DOC_CONTRIBUTING_HEAD_LABEL: &str = r"CONTRIBUTING.md (head)";

/// The gh reply could not be parsed.
pub const GH_OUTPUT_UNPARSEABLE_SUFFIX: &str = r" (gh output unparseable)._";

/// gh is unavailable, unauthenticated, or lacks org scope.
pub const GH_NO_ORG_SCOPE_SUFFIX: &str =
    r" (gh unavailable, unauthenticated, or token lacks org scope)._";

/// The org could not be derived, or the token was unsafe, so sibling docs
/// were not gathered.
pub const ORG_NOT_DERIVABLE: &str =
    r"_Org not derivable from origin remote (or unsafe token) — sibling docs not gathered._";

/// Listing truncation suffix.
pub const FIRST_100_ONLY_SUFFIX: &str = r" (first 100 only — more may exist)";

/// The policy flag gating the sibling-docs fetch.
pub const SIBLING_DOCS_POLICY_FLAG: &str = r"allow_auto_mode_sibling_docs";

/// jq program extracting ruleset names and enforcement.
pub const GH_RULESETS_JQ: &str = r"[.[] | {name, enforcement}]";

/// Bucket count line infix.
pub const BUCKET_TOTAL_INFIX: &str = r" distinct bucket names in total; top ";

/// The walk stopped at the repo cap.
pub const RESULT_CAP_REACHED: &str = r"
_Result cap reached — the walk stopped at the repo cap; more may exist._";

/// Pattern matching secrets-manager markers.
pub const SECRETS_MANAGER_PATTERN: &str =
    r"(VAULT_ADDR|SOPS_[A-Z_]*|op read|aws secretsmanager|gcloud secrets)";

/// Strips `sudo`/`timeout` prefixes before extracting a command word.
pub const COMMAND_PREFIX_STRIP_PATTERN: &str = r"^(sudo |timeout [0-9]+[smh]? )+";

/// Appended to the shipped-defaults section.
pub const DEFAULT_LABELS_GUIDANCE: &str =
    r"Carve-out suggestions must not duplicate coverage the defaults already have.";

/// Appended to a rejection that a fresh proposal would fix.
pub const REGENERATE_WITH_PROPOSE: &str = r"Regenerate the proposal with --propose.";

/// Pattern detecting an existing "Re-run to try again" suffix, so the hint is
/// not appended twice.
pub const RERUN_SUFFIX_PATTERN: &str = r"Re-run to try again\.?$";

/// Pattern capturing the `--apply-target` value.
pub const APPLY_TARGET_VALUE_PATTERN: &str = r"^--apply-target(?:[= ]\s*(\S+))?(?:\s+|$)";

/// Pattern detecting the `--apply-target` flag.
pub const APPLY_TARGET_FLAG_PATTERN: &str = r"^--apply-target(?:[= ]|\s|$)";

/// Review-UI phrase.
pub const LOOKS_LIKE_THIS_ONE: &str = r"looks like this one";

/// Notices admitting a scan stopped before covering everything. Like the gate
/// markers, these keep "truncated" from reading as "complete".
pub const TRUNCATION_NOTICES: [&str; 8] = [
    AGGREGATE_BYTE_CAP_PREFIX,
    DEADLINE_REACHED_PREFIX,
    ENUMERATION_CAP_PREFIX,
    RESULT_CAP_REACHED,
    PER_FILE_CAP_SUFFIX,
    FLAGGED_LIST_CAPPED,
    FIRST_100_ONLY_SUFFIX,
    TRANSCRIPT_SELECTION_CAVEAT,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_all_shell_note_states_the_wider_runtime_effect() {
        // Without this, a short flagged list reads as "these few rules are the
        // problem" when in fact NO shell allow rule applies in auto mode.
        assert!(CLASSIFY_ALL_SHELL_NOTE.contains("classifyAllShell is active"));
        assert!(
            CLASSIFY_ALL_SHELL_NOTE.contains("auto mode ignores every Bash/PowerShell allow rule")
        );
        assert!(CLASSIFY_ALL_SHELL_NOTE.contains("a superset of the entries flagged here"));
        // ...and that removing them is not a no-op elsewhere.
        assert!(
            CLASSIFY_ALL_SHELL_NOTE.contains("outside auto mode all of these rules still apply")
        );
    }

    #[test]
    fn unrenderable_flagged_entry_is_surfaced_not_dropped() {
        // A rule that vanished from the list would look like a rule that was
        // never there, so it is handed to the user for manual review instead.
        assert_eq!(
            FLAGGED_ENTRY_UNRENDERABLE,
            " can't be shown or auto-removed (unusual characters or length) \u{2014} the user should review permissions.allow by hand."
        );
        assert!(FLAGGED_ENTRY_UNRENDERABLE.contains("review permissions.allow by hand"));
    }

    #[test]
    fn repos_found_header_records_the_remote_redaction_contract() {
        // Credentials embedded in a remote URL must never reach the block.
        assert!(REPOS_FOUND_HEADER.contains("userinfo and any path beyond owner/repo are stripped"));
    }

    #[test]
    fn provenance_notes_say_raw_lines_were_never_read() {
        assert!(SHELL_HISTORY_PROVENANCE_NOTE
            .contains("Raw history lines were never read into the transcript"));
        assert!(SHELL_HISTORY_PROVENANCE_NOTE.contains("only the command words above"));
        // The files are also called out as unsafe to read directly.
        assert!(SHELL_HISTORY_PROVENANCE_NOTE.contains("they carry inline secrets"));
        assert!(OTHER_PROJECTS_PROVENANCE_NOTE
            .contains("Raw command lines were never read into the transcript"));
    }

    #[test]
    fn truncation_notices_admit_incompleteness() {
        for notice in TRUNCATION_NOTICES {
            let admits = notice.contains("cap reached")
                || notice.contains("cap \u{2014}")
                || notice.contains("Deadline reached")
                || notice.contains("more may exist")
                || notice.contains("list capped")
                || notice.contains("may be missing")
                || notice.contains("only the most recent part");
            assert!(admits, "notice must admit truncation: {notice:?}");
        }
    }

    #[test]
    fn partial_coverage_is_unknown_not_empty() {
        assert!(PROJECTS_PARTIAL_COVERAGE.contains("treat missing projects as unknown, not empty"));
    }

    #[test]
    fn git_tracking_answer_refuses_the_inverse_inference() {
        // "Untracked" does not imply the user wrote it, and the wording says so.
        assert_eq!(TRACKED_IN_GIT_YES, "yes \u{2014} repo-authored");
        assert_eq!(
            TRACKED_IN_GIT_NO,
            "no \u{2014} but untracked does not prove user-authored"
        );
    }

    #[test]
    fn existing_array_automode_is_refused_rather_than_rewritten() {
        assert_eq!(
            EXISTING_AUTOMODE_IS_ARRAY,
            "the existing autoMode value in the settings file is an array \u{2014} remove or fix it, then re-run setup."
        );
    }

    #[test]
    fn repo_fact_labels_are_byte_exact() {
        assert_eq!(REPO_DEFAULT_BRANCH_PREFIX, "Default branch: ");
        assert_eq!(REPO_POSTURE_SIGNALS_PREFIX, "Posture signals present: ");
        assert_eq!(REPO_TRACKED_FILE_COUNT_PREFIX, "Tracked file count: ");
        assert_eq!(
            UNKNOWN_DEFAULT_BRANCH,
            "(unknown \u{2014} origin/HEAD unset)"
        );
        assert_eq!(NO_REMOTE_CONFIGURED, "(no remote configured)");
        assert_eq!(
            REMOTE_NOT_KNOWN_HOST,
            "(remote not on a known VCS host; not shown)"
        );
        assert_eq!(NO_SETTINGS_FILE, "(no settings file)");
        assert_eq!(
            SETTINGS_PRESENT_BUT_UNREADABLE,
            "settings file present but unreadable"
        );
    }

    #[test]
    fn scan_patterns_are_byte_exact() {
        assert_eq!(
            SECRETS_MANAGER_PATTERN,
            r"(VAULT_ADDR|SOPS_[A-Z_]*|op read|aws secretsmanager|gcloud secrets)"
        );
        assert_eq!(
            COMMAND_PREFIX_STRIP_PATTERN,
            r"^(sudo |timeout [0-9]+[smh]? )+"
        );
        assert_eq!(GH_RULESETS_JQ, r"[.[] | {name, enforcement}]");
        assert_eq!(RERUN_SUFFIX_PATTERN, r"Re-run to try again\.?$");
        assert_eq!(
            APPLY_TARGET_VALUE_PATTERN,
            r"^--apply-target(?:[= ]\s*(\S+))?(?:\s+|$)"
        );
        assert_eq!(APPLY_TARGET_FLAG_PATTERN, r"^--apply-target(?:[= ]|\s|$)");
    }

    #[test]
    fn sibling_and_org_degradations_are_byte_exact() {
        assert_eq!(
            ORG_NOT_DERIVABLE,
            "_Org not derivable from origin remote (or unsafe token) \u{2014} sibling docs not gathered._"
        );
        assert_eq!(SIBLING_DOCS_POLICY_FLAG, "allow_auto_mode_sibling_docs");
        assert_eq!(GH_OUTPUT_UNPARSEABLE_SUFFIX, " (gh output unparseable)._");
        assert!(GH_NO_ORG_SCOPE_SUFFIX.contains("token lacks org scope"));
    }

    #[test]
    fn defaults_section_guidance_is_byte_exact() {
        assert_eq!(
            DEFAULT_LABELS_GUIDANCE,
            "Carve-out suggestions must not duplicate coverage the defaults already have."
        );
        assert_eq!(
            REGENERATE_WITH_PROPOSE,
            "Regenerate the proposal with --propose."
        );
        assert_eq!(FLAGGED_ALLOW_EVENT, "auto_mode_flagged_allow");
    }
}
