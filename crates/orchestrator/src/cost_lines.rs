//! Count added/removed lines from a tool result's `structuredPatch` — the
//! per-edit input to the session's cumulative code-change counters
//! (claude-code `Bhn(added, removed)`).

/// Sum `+`/`-` lines across a `structuredPatch` hunk array. A `+`-prefixed line
/// is an addition, `-` a removal; context lines (` `) and anything else are
/// ignored. Non-array / absent input yields `(0, 0)`.
#[cfg(test)]
pub(crate) fn count_structured_patch_lines(structured_patch: &serde_json::Value) -> (u64, u64) {
    let Some(hunks) = structured_patch.as_array() else {
        return (0, 0);
    };
    let mut added = 0u64;
    let mut removed = 0u64;
    for hunk in hunks {
        let Some(lines) = hunk.get("lines").and_then(|l| l.as_array()) else {
            continue;
        };
        for line in lines {
            match line.as_str().and_then(|s| s.chars().next()) {
                Some('+') => added += 1,
                Some('-') => removed += 1,
                _ => {}
            }
        }
    }
    (added, removed)
}

/// Source-native counterpart for provider/tool result payloads.
pub(crate) fn count_structured_patch_lines_source(
    structured_patch: &lingxi_core::types::utf16_json::Utf16JsonProjection,
) -> (u64, u64) {
    let Some(hunks) = structured_patch.array_len("") else {
        return (0, 0);
    };
    let mut added = 0u64;
    let mut removed = 0u64;
    for hunk in 0..hunks {
        let pointer = format!("/{hunk}/lines");
        for index in 0..structured_patch.array_len(&pointer).unwrap_or_default() {
            let Some(line) = structured_patch.string_units(&format!("{pointer}/{index}")) else {
                continue;
            };
            match line.first() {
                Some(unit) if *unit == b'+' as u16 => added += 1,
                Some(unit) if *unit == b'-' as u16 => removed += 1,
                _ => {}
            }
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_added_and_removed_across_hunks() {
        // structuredPatch hunks each carry a `lines` array; `+`/`-` prefixes.
        let patch = json!([
            { "lines": ["-old line", "+new line", " context", "+added2"] },
            { "lines": ["-gone"] }
        ]);
        assert_eq!(count_structured_patch_lines(&patch), (2, 2));
    }

    #[test]
    fn empty_or_non_array_is_zero() {
        assert_eq!(count_structured_patch_lines(&json!([])), (0, 0));
        assert_eq!(count_structured_patch_lines(&json!(null)), (0, 0));
    }
}
