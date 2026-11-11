//! Exact JavaScript text operations used at mutation and output boundaries.

use crate::Utf16JsonProjection;
use std::collections::HashMap;

pub(crate) fn string(units: &[u16]) -> Utf16JsonProjection {
    let mut builder = lingxi_core::types::utf16_json::Utf16JsonBuilder::new();
    let value = builder.push_string_units(units.to_vec());
    builder.finish(value).expect("exact string is valid JSON")
}

pub(crate) fn without_bom(units: &[u16]) -> &[u16] {
    units.strip_prefix(&[0xfeff]).unwrap_or(units)
}

/// Prefix table for exact UTF-16 matching. Building once keeps a sequence of
/// non-overlapping matches linear even when the needle has a long repeated
/// prefix, such as `aaaa...ab` in a file full of `a` units.
struct UnitNeedle<'a> {
    units: &'a [u16],
    prefix: Vec<usize>,
}

impl<'a> UnitNeedle<'a> {
    fn new(units: &'a [u16]) -> Self {
        let mut prefix = vec![0; units.len()];
        let mut matched = 0;
        for index in 1..units.len() {
            while matched > 0 && units[index] != units[matched] {
                matched = prefix[matched - 1];
            }
            if units[index] == units[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        Self { units, prefix }
    }

    fn matches<'m, 'h>(&'m self, haystack: &'h [u16]) -> UnitMatches<'m, 'a, 'h> {
        UnitMatches {
            needle: self,
            haystack,
            cursor: 0,
            matched: 0,
            #[cfg(test)]
            comparisons: 0,
        }
    }
}

struct UnitMatches<'m, 'n, 'h> {
    needle: &'m UnitNeedle<'n>,
    haystack: &'h [u16],
    cursor: usize,
    matched: usize,
    #[cfg(test)]
    comparisons: usize,
}

impl Iterator for UnitMatches<'_, '_, '_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.needle.units.is_empty() {
            return None;
        }
        while let Some(&unit) = self.haystack.get(self.cursor) {
            self.cursor += 1;
            loop {
                #[cfg(test)]
                {
                    self.comparisons += 1;
                }
                if self.needle.units[self.matched] == unit {
                    self.matched += 1;
                    break;
                }
                if self.matched == 0 {
                    break;
                }
                self.matched = self.needle.prefix[self.matched - 1];
            }
            if self.matched == self.needle.units.len() {
                // Edit uses JavaScript's non-overlapping replacement/count
                // semantics. The next scan begins at this match's end.
                self.matched = 0;
                return Some(self.cursor - self.needle.units.len());
            }
        }
        None
    }
}

pub(crate) fn find(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    if needle.is_empty() {
        Some(0)
    } else {
        UnitNeedle::new(needle).matches(haystack).next()
    }
}

pub(crate) fn count(haystack: &[u16], needle: &[u16]) -> usize {
    UnitNeedle::new(needle).matches(haystack).count()
}

pub(crate) fn replace(
    haystack: &[u16],
    needle: &[u16],
    replacement: &[u16],
    all: bool,
) -> Vec<u16> {
    if needle.is_empty() {
        let mut output = replacement.to_vec();
        output.extend_from_slice(haystack);
        return output;
    }
    let prepared = UnitNeedle::new(needle);
    let mut output = Vec::with_capacity(haystack.len());
    let mut copied = 0;
    for index in prepared.matches(haystack) {
        output.extend_from_slice(&haystack[copied..index]);
        output.extend_from_slice(replacement);
        copied = index + needle.len();
        if !all {
            break;
        }
    }
    output.extend_from_slice(&haystack[copied..]);
    output
}

pub(crate) fn input_error(
    prefix: &str,
    text: &[u16],
    suffix: &str,
) -> tool_api::tool_trait::ToolError {
    let units: Vec<u16> = prefix
        .encode_utf16()
        .chain(text.iter().copied())
        .chain(suffix.encode_utf16())
        .collect();
    tool_api::tool_trait::ToolError::InvalidInput(string(&units))
}

/// Intern entire exact lines before diffing. ASCII line IDs are a bijection,
/// including the trailing-newline bit, so Myers sees the same equality graph
/// without ever replacing unpaired UTF-16 units. Restore each hunk line from
/// its source after the existing jsdiff hunk/context algorithm completes.
pub(crate) fn patch(before: &[u16], after: &[u16]) -> (Utf16JsonProjection, u64, u64) {
    let mut ids: HashMap<Vec<u16>, usize> = HashMap::new();
    let mut source = Vec::<Vec<u16>>::new();
    let mut intern = |text: &[u16]| {
        let mut encoded = String::new();
        for line in text.split_inclusive(|unit| *unit == 10) {
            let id = *ids.entry(line.to_vec()).or_insert_with(|| {
                let id = source.len();
                source.push(line.to_vec());
                id
            });
            encoded.push_str(&id.to_string());
            if line.last() == Some(&10) {
                encoded.push('\n');
            }
        }
        encoded
    };
    let before = intern(before);
    let after = intern(after);
    let hunks = crate::structured_patch::build_structured_patch(&before, &after);
    let (added, removed) = crate::structured_patch::count_patch_lines(&hunks);
    let mut data = crate::generated_result(&serde_json::json!(hunks));
    for (i, hunk) in hunks.iter().enumerate() {
        for (j, line) in hunk.lines.iter().enumerate() {
            if line.starts_with('\\') {
                continue;
            }
            let id = line[1..].parse::<usize>().expect("interned patch line ID");
            let original = source[id].strip_suffix(&[10]).unwrap_or(&source[id]);
            let mut units = vec![u16::from(line.as_bytes()[0])];
            units.extend_from_slice(original);
            data.set_string_units(&format!("/{i}/lines/{j}"), &units)
                .expect("patch line exists");
        }
    }
    (data, added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_matches_remain_linear_on_repeated_prefix_misses() {
        let haystack = vec![b'a' as u16; 100_000];
        let mut needle = vec![b'a' as u16; 50_000];
        needle.push(b'b' as u16);
        let prepared = UnitNeedle::new(&needle);
        let mut matches = prepared.matches(&haystack);
        assert_eq!(matches.next(), None);
        assert!(matches.comparisons <= haystack.len() * 2);
        assert_eq!(count(&haystack, &needle), 0);
        assert_eq!(replace(&haystack, &needle, &[0xd800], true), haystack);
    }

    #[test]
    fn unit_matches_preserve_nonoverlap_and_exact_surrogate_units() {
        assert_eq!(find(&[], &[]), Some(0));
        assert_eq!(count(&[1, 2], &[]), 0);
        assert_eq!(replace(&[1, 2], &[], &[3], true), vec![3, 1, 2]);
        assert_eq!(count(&[1, 1, 1], &[1, 1]), 1);
        assert_eq!(replace(&[1, 1, 1], &[1, 1], &[2], true), vec![2, 1]);
        let source = [0xd800, 10, 0xd801, 10, 0xd800, 10];
        assert_eq!(find(&source, &[0xd801]), Some(2));
        assert_eq!(count(&source, &[0xd800]), 2);
        assert_eq!(
            replace(&source, &[0xd800], &[0xdc00], false),
            vec![0xdc00, 10, 0xd801, 10, 0xd800, 10]
        );
        assert_eq!(
            replace(&source, &[0xd800], &[0xdc00], true),
            vec![0xdc00, 10, 0xd801, 10, 0xdc00, 10]
        );
    }

    #[test]
    fn unit_matches_agree_with_exhaustive_short_sequence_oracle() {
        let words = (0..=6)
            .flat_map(|length| {
                (0..1usize << length).map(move |bits| {
                    (0..length)
                        .map(|bit| {
                            if bits & (1 << bit) == 0 {
                                0xd800
                            } else {
                                0xd801
                            }
                        })
                        .collect::<Vec<u16>>()
                })
            })
            .collect::<Vec<_>>();
        for haystack in &words {
            for needle in &words {
                let expected_find = if needle.is_empty() {
                    Some(0)
                } else {
                    haystack
                        .windows(needle.len())
                        .position(|window| window == needle)
                };
                assert_eq!(
                    find(haystack, needle),
                    expected_find,
                    "find {haystack:?} / {needle:?}"
                );
                if needle.is_empty() {
                    continue;
                }
                let mut rest = haystack.as_slice();
                let mut expected_count = 0;
                let mut expected_replace = Vec::new();
                while let Some(index) = rest
                    .windows(needle.len())
                    .position(|window| window == needle)
                {
                    expected_count += 1;
                    expected_replace.extend_from_slice(&rest[..index]);
                    expected_replace.push(10);
                    rest = &rest[index + needle.len()..];
                }
                expected_replace.extend_from_slice(rest);
                assert_eq!(count(haystack, needle), expected_count);
                assert_eq!(replace(haystack, needle, &[10], true), expected_replace);
            }
        }
    }

    #[test]
    fn exact_surrogate_lines_are_distinct_in_patch() {
        let (patch, added, removed) = patch(&[0xd800, 10], &[0xd801, 10]);
        assert_eq!((added, removed), (1, 1));
        assert_eq!(patch.string_units("/0/lines/0"), Some(vec![45, 0xd800]));
        assert_eq!(patch.string_units("/0/lines/1"), Some(vec![43, 0xd801]));
    }

    #[test]
    fn interned_patch_matches_existing_scalar_hunks() {
        for (before, after) in [
            ("a\nb\nc\n", "a\nB\nc\n"),
            ("", "hello"),
            ("x\n\ny", "x\nY\n"),
            ("a\nb", "a\nB"),
        ] {
            let (actual, _, _) = patch(
                &before.encode_utf16().collect::<Vec<_>>(),
                &after.encode_utf16().collect::<Vec<_>>(),
            );
            let expected = crate::generated_result(&serde_json::json!(
                crate::structured_patch::build_structured_patch(before, after)
            ));
            assert_eq!(
                actual.to_json_string().unwrap(),
                expected.to_json_string().unwrap()
            );
        }
    }
}
