//! Validate the reserved citation syntax in a merged answer against the panel
//! evidence ids actually delivered in the final synthesis payload.
//!
//! This is a purely local integrity check on references. It asserts that a
//! cited id was really listed for that panel in the request the synthesizer
//! saw. It does NOT assert that the evidence was fetched, that the panel read
//! what it claims, or that the surrounding interpretation is true.

use std::collections::BTreeSet;

const PREFIX: &[u8] = b"[evidence:";
/// `P1` … `P99`: the anonymous panel ids Fusion assigns after the shuffle.
const MAX_PANEL_DIGITS: usize = 2;
/// Report-local evidence ids are model-authored, so they are bounded here
/// rather than trusted. Anything longer is malformed, not unauthorized.
const MAX_EVIDENCE_ID_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CitationValidation {
    /// No reserved citation marker appeared. This is NOT positive evidence
    /// verification and must remain distinguishable in the caller's result.
    NoCitations,
    /// Every cited reference was delivered; duplicates are removed. A caller
    /// must not interpret this as verification of the surrounding claims.
    ValidReferences { references: BTreeSet<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CitationError {
    /// A reserved marker is not exactly `[evidence:P<n>:<id>]`.
    /// Offsets are UTF-8 byte offsets into the unchanged answer.
    Malformed { offset: usize },
    /// Syntax is valid, but the reference is absent from the final delivered
    /// allowlist (including unknown, cross-panel, and packed-out references).
    Unauthorized { offset: usize },
}

/// `allowed_refs` must hold `"<panel_id>:<evidence_id>"` for every evidence
/// entry actually serialized into the FINAL synthesis request, after packing.
/// A panel whose report was trimmed out of the packed payload contributes
/// nothing, so a reference to it is unauthorized rather than silently accepted.
///
/// Quoted prose, code fences and backslashes do not disable this reserved
/// syntax: an unknown reference cannot bypass validation by being quoted.
/// Ordinary URLs, plain ids and other Markdown links are ignored; they cannot
/// produce a positive citation-validation result.
///
/// The scan is strictly linear in answer bytes, apart from ordered-set lookup
/// and insertion. Each position has one constant-length prefix check; each
/// recognized marker examines at most 68 further bytes and then advances past
/// it. There is no suffix search, regex backtracking, or repeated whole scan.
pub(crate) fn validate_merged_citations(
    answer: &str,
    allowed_refs: &BTreeSet<String>,
) -> Result<CitationValidation, CitationError> {
    let bytes = answer.as_bytes();
    let mut offset = 0;
    let mut references = BTreeSet::new();
    while offset < bytes.len() {
        if !bytes[offset..].starts_with(PREFIX) {
            offset += 1;
            continue;
        }
        let body_start = offset + PREFIX.len();
        let Some(body_end) = scan_reference(bytes, body_start) else {
            return Err(CitationError::Malformed { offset });
        };
        // Every byte accepted by `scan_reference` is ASCII, so both indices
        // sit on UTF-8 boundaries even when surrounding text contains CJK.
        let reference = &answer[body_start..body_end];
        if !allowed_refs.contains(reference) {
            return Err(CitationError::Unauthorized { offset });
        }
        references.insert(reference.to_owned());
        offset = body_end + 1;
    }
    if references.is_empty() {
        Ok(CitationValidation::NoCitations)
    } else {
        Ok(CitationValidation::ValidReferences { references })
    }
}

/// Exclusive end of a well-formed `P<n>:<id>` body, whose closing bracket sits
/// at exactly that index. `None` for any other shape. The cursor only ever
/// moves forward and examines a bounded number of bytes.
fn scan_reference(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    if *bytes.get(index)? != b'P' {
        return None;
    }
    index += 1;
    let digits_start = index;
    while index - digits_start < MAX_PANEL_DIGITS
        && bytes.get(index).is_some_and(u8::is_ascii_digit)
    {
        index += 1;
    }
    // `P0`, `P01` and a bare `P` are all malformed: panel ids start at P1 and
    // carry no leading zero, so there is exactly one spelling per panel.
    if index == digits_start || bytes[digits_start] == b'0' {
        return None;
    }
    if *bytes.get(index)? != b':' {
        return None;
    }
    index += 1;
    let id_start = index;
    while index - id_start < MAX_EVIDENCE_ID_BYTES
        && bytes
            .get(index)
            .is_some_and(|byte| is_evidence_id_byte(*byte))
    {
        index += 1;
    }
    if index == id_start || *bytes.get(index)? != b']' {
        return None;
    }
    Some(index)
}

const fn is_evidence_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "P1:e1";
    const B: &str = "P2:src-2.rs_v1";

    fn allow(references: &[&str]) -> BTreeSet<String> {
        references
            .iter()
            .map(|reference| (*reference).into())
            .collect()
    }

    #[test]
    fn allows_only_exact_delivered_references_and_deduplicates() {
        let text = format!("[evidence:{B}] [evidence:{A}][evidence:{B}]");
        assert_eq!(
            validate_merged_citations(&text, &allow(&[A, B])),
            Ok(CitationValidation::ValidReferences {
                references: allow(&[A, B])
            })
        );
    }

    #[test]
    fn unknown_cross_panel_or_packed_out_reference_is_unauthorized() {
        let text = format!("[evidence:{B}]");
        // These provenance failures intentionally share one safe error: only
        // the host's final delivered allowlist can distinguish their origins.
        // The middle case is the packed-out one: panel 2's report was trimmed
        // out of the final payload, so its ids are no longer citable.
        for allowed in [allow(&[]), allow(&[A]), allow(&["P2:src-2"])] {
            assert_eq!(
                validate_merged_citations(&text, &allowed),
                Err(CitationError::Unauthorized { offset: 0 })
            );
        }
    }

    #[test]
    fn a_reference_is_bound_to_the_panel_that_delivered_it() {
        // Same evidence id, different panel: citing it under the wrong panel
        // is a provenance failure, not a match.
        let text = "[evidence:P2:e1]";
        assert_eq!(
            validate_merged_citations(text, &allow(&["P1:e1"])),
            Err(CitationError::Unauthorized { offset: 0 })
        );
    }

    #[test]
    fn malformed_empty_nested_unclosed_or_noncanonical_markers_fail() {
        let long_id = "e".repeat(MAX_EVIDENCE_ID_BYTES + 1);
        let malformed = [
            "[evidence:".to_string(),
            "[evidence:]".into(),
            "[evidence:P1]".into(),
            "[evidence:P1:]".into(),
            "[evidence::e1]".into(),
            format!("[evidence:{A}"),
            format!("[evidence: {A}]"),
            format!("[evidence:{A} ]"),
            "[evidence:p1:e1]".into(),
            "[evidence:P0:e1]".into(),
            "[evidence:P01:e1]".into(),
            "[evidence:P100:e1]".into(),
            format!("[evidence:P1:{long_id}]"),
            "[evidence:P1:e 1]".into(),
            "[evidence:P1:e/1]".into(),
            "[evidence:P1:e中]".into(),
            "[evidence:P1:e\n1]".into(),
            format!("[evidence:[evidence:{A}]]"),
        ];
        for text in malformed {
            assert_eq!(
                validate_merged_citations(&text, &allow(&[A])),
                Err(CitationError::Malformed { offset: 0 }),
                "{text}"
            );
        }
    }

    #[test]
    fn an_id_at_the_length_limit_is_still_well_formed() {
        let id = "e".repeat(MAX_EVIDENCE_ID_BYTES);
        let reference = format!("P9:{id}");
        let text = format!("[evidence:{reference}]");
        assert_eq!(
            validate_merged_citations(&text, &allow(&[&reference])),
            Ok(CitationValidation::ValidReferences {
                references: allow(&[&reference])
            })
        );
    }

    #[test]
    fn no_citation_is_explicitly_not_verified() {
        for text in [
            "",
            "\u{666e}\u{901a}\u{65e0}\u{5f15}\u{7528}\u{7684}\u{7b54}\u{6848} \u{1f9ea}",
            A,
            "https://example.invalid/P1:e1",
            "[ordinary markdown](https://example.invalid)",
        ] {
            assert_eq!(
                validate_merged_citations(text, &allow(&[A])),
                Ok(CitationValidation::NoCitations)
            );
        }
    }

    #[test]
    fn quote_code_or_escape_context_does_not_bypass_validation() {
        for (prefix, suffix) in [("\"", "\""), ("```\n", "\n```"), ("\\", ""), ("\'", "\'")] {
            let text = format!("{prefix}[evidence:{B}]{suffix}");
            assert_eq!(
                validate_merged_citations(&text, &allow(&[A])),
                Err(CitationError::Unauthorized {
                    offset: prefix.len()
                })
            );
            assert!(matches!(
                validate_merged_citations(&text, &allow(&[B])),
                Ok(CitationValidation::ValidReferences { .. })
            ));
        }
    }

    #[test]
    fn cjk_byte_offsets_and_later_invalid_marker_are_preserved() {
        let prefix = format!("\u{7ed3}\u{8bba}\u{1f9ea}[evidence:{A}] \u{63a5}\u{7740}\u{ff1a}");
        let text = format!("{prefix}[evidence:{B}]");
        assert_eq!(
            validate_merged_citations(&text, &allow(&[A])),
            Err(CitationError::Unauthorized {
                offset: prefix.len()
            })
        );
        let text = format!("{prefix}[evidence:]");
        assert_eq!(
            validate_merged_citations(&text, &allow(&[A])),
            Err(CitationError::Malformed {
                offset: prefix.len()
            })
        );
    }

    #[test]
    fn repeated_near_prefixes_and_duplicate_markers_remain_bounded() {
        // No timing assertion: stress repeated near-matches without a regex
        // engine or suffix searches, then verify exact deduplication.
        let text = format!(
            "{}{}",
            "[evidenc \u{1f9ea}".repeat(10_000),
            format!("[evidence:{A}]").repeat(10_000)
        );
        assert_eq!(
            validate_merged_citations(&text, &allow(&[A])),
            Ok(CitationValidation::ValidReferences {
                references: allow(&[A])
            })
        );
    }
}
