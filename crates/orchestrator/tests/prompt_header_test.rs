//! Prompt headers preserve their locked text.

use orchestrator::prompt::locked_templates::{HEADER, SECTION_SEP, TRAILING_NL};

#[test]
fn header_starts_with_you_are_lingxi() {
    assert!(HEADER.starts_with("You are LingXi"));
}

#[test]
fn header_describes_lingxi_identity() {
    assert!(HEADER.contains("agentic command-line coding assistant"));
}

#[test]
fn header_has_locked_byte_length() {
    // 57 bytes — see plan reverse-engineered byte-locks table.
    // If this fails after a claude-code rebase, re-verify with
    // `printf '%s' "..." | wc -c` against the new DEFAULT_PREFIX.
    assert_eq!(
        HEADER.len(),
        57,
        "HEADER byte length must match locked value"
    );
}

#[test]
fn header_has_no_leading_or_trailing_whitespace() {
    assert_eq!(HEADER.trim(), HEADER);
}

#[test]
fn header_does_not_contain_newline() {
    assert!(!HEADER.contains('\n'));
}

#[test]
fn section_sep_is_two_lf() {
    assert_eq!(SECTION_SEP, "\n\n");
    assert_eq!(SECTION_SEP.len(), 2);
}

#[test]
fn trailing_nl_is_single_lf() {
    assert_eq!(TRAILING_NL, "\n");
    assert_eq!(TRAILING_NL.len(), 1);
}
