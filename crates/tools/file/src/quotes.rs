//! Curly-quote normalization for Edit — port of
//! `FileEditTool/utils.ts:21-37` (curly constants + `normalizeQuotes`),
//! `:73-93` (`findActualString`), `:104-199`
//! (`preserveQuoteStyle` / `isOpeningContext` / `applyCurlyDoubleQuotes` /
//! `applyCurlySingleQuotes`, including the contraction rule).
//!
//! Claude cannot emit curly quotes (they are sanitized out of the API), so the
//! model always sends straight quotes. When a file contains typographic
//! (curly) quotes, an exact `old_string` match fails. [`find_actual_string`]
//! recovers the real curly text from the file so the edit still locates its
//! target, and [`preserve_quote_style`] re-applies the file's curly typography
//! to `new_string` so the rewrite does not silently strip curly quotes.
//!
//! Matching and escape preservation follow the native UTF-16 representation.
//! Exact-unit helpers retain unpaired surrogates without a scalar facade.
//! The `String` convenience wrappers expose display text only; mutations use
//! the exact-unit APIs for matching, escaping, and replacement.

/// Left single curly quote (U+2018). Mirrors `LEFT_SINGLE_CURLY_QUOTE`.
pub const LEFT_SINGLE_CURLY_QUOTE: char = '\u{2018}';
/// Right single curly quote (U+2019). Mirrors `RIGHT_SINGLE_CURLY_QUOTE`.
pub const RIGHT_SINGLE_CURLY_QUOTE: char = '\u{2019}';
/// Left double curly quote (U+201C). Mirrors `LEFT_DOUBLE_CURLY_QUOTE`.
pub const LEFT_DOUBLE_CURLY_QUOTE: char = '\u{201C}';
/// Right double curly quote (U+201D). Mirrors `RIGHT_DOUBLE_CURLY_QUOTE`.
pub const RIGHT_DOUBLE_CURLY_QUOTE: char = '\u{201D}';

/// Normalize quotes in a string by converting curly quotes to straight quotes.
///
/// Port of `normalizeQuotes` (`utils.ts:31-37`).
#[must_use]
pub fn normalize_quotes(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            LEFT_SINGLE_CURLY_QUOTE | RIGHT_SINGLE_CURLY_QUOTE => '\'',
            LEFT_DOUBLE_CURLY_QUOTE | RIGHT_DOUBLE_CURLY_QUOTE => '"',
            other => other,
        })
        .collect()
}

/// Find the actual string in `file` that matches `search`, accounting for quote
/// normalization AND the `\uXXXX`-escape-swap / non-ASCII regex fallbacks.
///
/// Byte-faithful port of the binary helper `vIe(e,t)` (offset 201327133):
///
/// ```text
/// function vIe(e,t){
///   if(e.includes(t))return t;                                 // (1) exact
///   let n=uUa(t),o=uUa(e).indexOf(n);                          // (2) curly
///   if(o!==-1)return e.substring(o,o+t.length);
///   if(Tlo.test(t)){let s=flo(t);if(s!==t&&e.includes(s))return s}  // (3) escape-swap
///   if(Slo.test(t)){let s=e.match(new RegExp(dUa(t)));if(s)return s[0]}  // (4) non-ASCII
///   return null
/// }
/// ```
///
/// where `uUa` curly-normalizes (`normalizeQuotes`), `Tlo=/\\u[0-9a-fA-F]{4}/`,
/// `Slo=/[-￿]/`, `flo` decodes `\uXXXX` escapes to chars (preserving
/// doubled `\\`), and `dUa` builds a regex matching the **escaped** `\uXXXX`
/// form of each non-ASCII codepoint (case-insensitive on a-f hex) and the
/// regex-escaped literal of each ASCII char.
///
/// Order matters: exact → curly-normalize → escape-decode → non-ASCII-escape
/// regex. Each fallback only runs if the prior one did not match. Returns `None`
/// when nothing matches (the binary's `return null`).
///
/// This UTF-8 convenience wrapper delegates to the exact-unit implementation.
/// A recovered string that is not representable as UTF-8 is never substituted
/// with U+FFFD and mistaken for different source text.
#[must_use]
pub fn find_actual_string(file: &str, search: &str) -> Option<String> {
    let file = file.encode_utf16().collect::<Vec<_>>();
    let search = search.encode_utf16().collect::<Vec<_>>();
    String::from_utf16(&find_actual_string_units(&file, &search)?).ok()
}

/// `Tlo.test(e)` where `Tlo=/\\u[0-9a-fA-F]{4}/` — true if `e` contains a
/// `\uXXXX` escape sequence (a literal backslash, `u`, then four hex digits).
/// Note this is the JS `RegExp.test` semantics (a *search*, not anchored): the
/// escape may appear anywhere, including after an even or odd run of backslashes
/// (the binary's `Tlo` has no backslash-pairing lookbehind, so neither do we).
#[must_use]
pub fn contains_unicode_escape(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 6 {
        return false;
    }
    for i in 0..=bytes.len() - 6 {
        if bytes[i] == b'\\'
            && bytes[i + 1] == b'u'
            && bytes[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit)
        {
            return true;
        }
    }
    false
}

/// Native non-Unicode regex `/[\u0080-\uffff]/`: tests UTF-16 units,
/// including both surrogate units of an astral scalar.
#[must_use]
pub fn contains_non_ascii_bmp(s: &str) -> bool {
    // The native regex has no `u` flag, so an astral scalar contributes two
    // matching surrogate units too.
    s.encode_utf16().any(|unit| unit >= 128)
}

/// `pUa(e)=Tlo.test(e)||Slo.test(e)` (offset 201326520): true when `e` either
/// contains a `\uXXXX` escape OR a non-ASCII BMP char. Used on the not-found
/// path to decide whether to append the escape-swap note.
#[must_use]
pub fn has_escape_or_non_ascii(s: &str) -> bool {
    contains_unicode_escape(s) || contains_non_ascii_bmp(s)
}

/// Byte-faithful port of `flo` (offset 201326240):
///
/// ```text
/// e.replace(/(\\\\)|\\u([0-9a-fA-F]{4})/g,(t,n,r)=>n!==void 0?t:String.fromCharCode(parseInt(r,16)))
/// ```
///
/// Left-to-right global scan: a literal doubled backslash `\\` is consumed
/// verbatim (so it is NOT interpreted as the start of an escape), otherwise a
/// `\uXXXX` sequence is decoded to its char via `String.fromCharCode` (a single
/// UTF-16 code unit — for the BMP this is one Rust char). Any text not matching
/// either alternative is copied unchanged.
fn decode_unicode_escapes(s: &str) -> String {
    String::from_utf16_lossy(&decode_unicode_escape_units(
        &s.encode_utf16().collect::<Vec<_>>(),
    ))
}

/// When `old_string` matched via quote normalization (curly quotes in file,
/// straight quotes from model), apply the same curly quote style to `new_string`
/// so the edit preserves the file's typography. Port of `preserveQuoteStyle`
/// (`utils.ts:104-136`).
#[must_use]
pub fn preserve_quote_style(old_string: &str, actual_old_string: &str, new_string: &str) -> String {
    String::from_utf16_lossy(&preserve_quote_style_units(
        &old_string.encode_utf16().collect::<Vec<_>>(),
        &actual_old_string.encode_utf16().collect::<Vec<_>>(),
        &new_string.encode_utf16().collect::<Vec<_>>(),
    ))
}

/// Native `qSr`: a match recovered by exchanging Unicode escapes and literal
/// characters preserves that representation in the replacement as well.
#[must_use]
pub fn preserve_escape_style(old: &str, actual: &str, replacement: &str) -> String {
    String::from_utf16_lossy(&preserve_escape_style_units(
        &old.encode_utf16().collect::<Vec<_>>(),
        &actual.encode_utf16().collect::<Vec<_>>(),
        &replacement.encode_utf16().collect::<Vec<_>>(),
    ))
}

/// Exact JavaScript-unit counterpart of [`normalize_quotes`].
#[must_use]
pub fn normalize_quotes_units(text: &[u16]) -> Vec<u16> {
    text.iter()
        .map(|unit| match unit {
            0x2018 | 0x2019 => 39,
            0x201c | 0x201d => 34,
            _ => *unit,
        })
        .collect()
}

fn find_units(file: &[u16], search: &[u16]) -> Option<usize> {
    crate::utf16_mutation::find(file, search)
}

/// Test native Unicode escape syntax without converting lone surrogates.
#[must_use]
pub fn contains_unicode_escape_units(text: &[u16]) -> bool {
    text.windows(6).any(|part| {
        part[0] == 92 && part[1] == 117 && part[2..].iter().all(|unit| ascii_hex(*unit).is_some())
    })
}

/// Native escape-swap diagnostic predicate over exact JavaScript units.
#[must_use]
pub fn has_escape_or_non_ascii_units(text: &[u16]) -> bool {
    contains_unicode_escape_units(text) || text.iter().any(|unit| *unit >= 128)
}

fn ascii_hex(unit: u16) -> Option<u16> {
    match unit {
        48..=57 => Some(unit - 48),
        65..=70 => Some(unit - 65 + 10),
        97..=102 => Some(unit - 97 + 10),
        _ => None,
    }
}

fn decode_unicode_escape_units(text: &[u16]) -> Vec<u16> {
    let mut output = Vec::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        if text[index] == 92 && text.get(index + 1) == Some(&92) {
            output.extend_from_slice(&text[index..index + 2]);
            index += 2;
        } else if text[index] == 92
            && text.get(index + 1) == Some(&117)
            && text
                .get(index + 2..index + 6)
                .is_some_and(|hex| hex.iter().all(|unit| ascii_hex(*unit).is_some()))
        {
            let unit = text[index + 2..index + 6].iter().fold(0, |value, digit| {
                value * 16 + ascii_hex(*digit).expect("hex unit")
            });
            output.push(unit);
            index += 6;
        } else {
            output.push(text[index]);
            index += 1;
        }
    }
    output
}

fn match_escaped_form_units(file: &[u16], search: &[u16]) -> Option<Vec<u16>> {
    use grep_matcher::Matcher;
    if search.iter().rev().take_while(|unit| **unit == 92).count() % 2 == 1 {
        return None;
    }
    let mut pattern = String::new();
    let mut boundaries = Vec::new();
    let mut width = 0;
    if search.first() == Some(&92) {
        boundaries.push(0);
    }
    for unit in search {
        if *unit >= 128 {
            boundaries.push(width);
            pattern.push_str(r"\x5c\x75");
            for digit in format!("{unit:04x}").chars() {
                if digit.is_ascii_alphabetic() {
                    pattern.push('[');
                    pattern.push(digit);
                    pattern.push(digit.to_ascii_uppercase());
                    pattern.push(']');
                } else {
                    pattern.push(digit);
                }
            }
            width += 6;
        } else {
            pattern.push_str(&format!(r"\x{unit:02x}"));
            width += 1;
        }
    }
    if width == 0 {
        return Some(Vec::new());
    }
    let matcher = match grep_regex::RegexMatcherBuilder::new()
        .line_terminator(None)
        .build(&pattern)
    {
        Ok(matcher) => matcher,
        // Preserve matching semantics for a pattern exceeding the automaton's
        // compilation budget; the bounded-memory direct matcher is still valid.
        Err(_) => return match_escaped_form_units_direct(file, search),
    };
    // The escaped-form pattern is entirely ASCII. Display repair of malformed
    // surrogates only emits non-ASCII U+FFFD and therefore cannot invent a match.
    // UTF-16 lengths before each ASCII match remain identical to the source.
    let display = String::from_utf16_lossy(file);
    let mut slash_parity = Vec::with_capacity(file.len() + 1);
    slash_parity.push(false);
    for unit in file {
        slash_parity.push(*unit == 92 && !slash_parity.last().copied().unwrap_or(false));
    }
    let mut at = 0;
    let mut byte_cursor = 0;
    let mut unit_cursor = 0;
    while let Some(found) = matcher.find_at(display.as_bytes(), at).ok()? {
        unit_cursor += display[byte_cursor..found.start()].encode_utf16().count();
        byte_cursor = found.start();
        if boundaries
            .iter()
            .all(|offset| !slash_parity[unit_cursor + offset])
        {
            let actual = file[unit_cursor..unit_cursor + width].to_vec();
            return (find_units(file, &actual) == Some(unit_cursor)).then_some(actual);
        }
        // The match starts with ASCII, so advancing one byte stays on a UTF-8
        // boundary while allowing overlapping candidates after a rejected one.
        at = found.start() + 1;
    }
    None
}

fn match_escaped_form_units_direct(file: &[u16], search: &[u16]) -> Option<Vec<u16>> {
    if search.iter().rev().take_while(|unit| **unit == 92).count() % 2 == 1 {
        return None;
    }
    let mut pattern = Vec::new();
    let mut boundaries = Vec::new();
    if search.first() == Some(&92) {
        boundaries.push(0);
    }
    for unit in search {
        if *unit >= 128 {
            boundaries.push(pattern.len());
            pattern.extend([(92, false), (117, false)]);
            pattern.extend(
                format!("{unit:04x}")
                    .encode_utf16()
                    .map(|digit| (digit, true)),
            );
        } else {
            pattern.push((*unit, false));
        }
    }
    if pattern.is_empty() {
        return Some(Vec::new());
    }
    for (start, candidate) in file.windows(pattern.len()).enumerate() {
        let matches = pattern.iter().zip(candidate).all(|((want, in_hex), got)| {
            if *in_hex {
                ascii_hex(*want) == ascii_hex(*got)
            } else {
                want == got
            }
        });
        if !matches {
            continue;
        }
        if !boundaries.iter().all(|offset| {
            file[..start + offset]
                .iter()
                .rev()
                .take_while(|unit| **unit == 92)
                .count()
                % 2
                == 0
        }) {
            continue;
        }
        let actual = candidate.to_vec();
        return (find_units(file, &actual) == Some(start)).then_some(actual);
    }
    None
}

/// Match exact UTF-16 content, then the native curly-quote and Unicode-escape
/// fallbacks. No scalar substitutions or sentinel alphabet are involved.
#[must_use]
pub fn find_actual_string_units(file: &[u16], search: &[u16]) -> Option<Vec<u16>> {
    if find_units(file, search).is_some() {
        return Some(search.to_vec());
    }
    let normalized_search = normalize_quotes_units(search);
    let normalized_file = normalize_quotes_units(file);
    if let Some(start) = find_units(&normalized_file, &normalized_search) {
        return Some(file[start..start + search.len()].to_vec());
    }
    if contains_unicode_escape_units(search) {
        let decoded = decode_unicode_escape_units(search);
        if decoded != search && find_units(file, &decoded).is_some() {
            return Some(decoded);
        }
    }
    if search.iter().any(|unit| *unit >= 128) {
        return match_escaped_form_units(file, search);
    }
    None
}

/// Preserve curly typography while retaining all original surrogate units.
#[must_use]
pub fn preserve_quote_style_units(old: &[u16], actual: &[u16], replacement: &[u16]) -> Vec<u16> {
    if old == actual {
        return replacement.to_vec();
    }
    let double = actual.iter().any(|unit| matches!(unit, 0x201c | 0x201d));
    let single = actual.iter().any(|unit| matches!(unit, 0x2018 | 0x2019));
    let opening = |text: &[u16], index: usize| {
        index == 0
            || matches!(
                text[index - 1],
                32 | 9 | 10 | 13 | 40 | 91 | 123 | 0x2014 | 0x2013
            )
    };
    let alphabetic = |unit: u16| char::from_u32(u32::from(unit)).is_some_and(char::is_alphabetic);
    let mut output = replacement.to_vec();
    if double {
        for index in 0..replacement.len() {
            if replacement[index] == 34 {
                output[index] = if opening(replacement, index) {
                    0x201c
                } else {
                    0x201d
                };
            }
        }
    }
    if single {
        let before = output.clone();
        for index in 0..before.len() {
            if before[index] == 39 {
                let contraction = index > 0
                    && index + 1 < before.len()
                    && alphabetic(before[index - 1])
                    && alphabetic(before[index + 1]);
                output[index] = if !contraction && opening(&before, index) {
                    0x2018
                } else {
                    0x2019
                };
            }
        }
    }
    output
}

/// Preserve native escaped/literal spelling using exact UTF-16 units.
#[must_use]
pub fn preserve_escape_style_units(old: &[u16], actual: &[u16], replacement: &[u16]) -> Vec<u16> {
    if old == actual {
        return replacement.to_vec();
    }
    if old.iter().any(|unit| *unit >= 128)
        && match_escaped_form_units(actual, old).as_deref() == Some(actual)
    {
        let mut spellings = std::collections::HashMap::new();
        let mut position = 0;
        let mut uppercase = 0;
        let mut lowercase = 0;
        for unit in old {
            if *unit >= 128 {
                let Some(hex) = actual.get(position + 2..position + 6) else {
                    return replacement.to_vec();
                };
                uppercase += hex
                    .iter()
                    .filter(|digit| (65..=70).contains(*digit))
                    .count();
                lowercase += hex
                    .iter()
                    .filter(|digit| (97..=102).contains(*digit))
                    .count();
                spellings.insert(*unit, hex);
                position += 6;
            } else {
                position += 1;
            }
        }
        if position != actual.len() {
            return replacement.to_vec();
        }
        let mut output = Vec::new();
        for unit in replacement {
            if *unit < 128 {
                output.push(*unit);
            } else if let Some(hex) = spellings.get(unit) {
                output.extend([92, 117]);
                output.extend_from_slice(hex);
            } else {
                output.extend(
                    if uppercase > lowercase {
                        format!("\\u{unit:04X}")
                    } else {
                        format!("\\u{unit:04x}")
                    }
                    .encode_utf16(),
                );
            }
        }
        return output;
    }
    if contains_unicode_escape_units(old) && decode_unicode_escape_units(old) == actual {
        return decode_unicode_escape_units(replacement);
    }
    replacement.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_322_lossless_matching_and_escape_style() {
        let fixture =
            crate::Utf16JsonProjection::parse(include_str!("fixtures/utf16_native_322.json"))
                .unwrap();
        for index in 0..fixture.array_len("/edits").unwrap() {
            let prefix = format!("/edits/{index}");
            let file = fixture.string_units(&format!("{prefix}/file")).unwrap();
            let old = fixture.string_units(&format!("{prefix}/old")).unwrap();
            let actual = find_actual_string_units(&file, &old);
            assert_eq!(
                actual,
                fixture.string_units(&format!("{prefix}/expected/actual")),
                "case {index}"
            );
            if let Some(actual) = actual {
                let replacement = fixture
                    .string_units(&format!("{prefix}/replacement"))
                    .unwrap();
                let replacement = preserve_escape_style_units(
                    &old,
                    &actual,
                    &preserve_quote_style_units(&old, &actual, &replacement),
                );
                assert_eq!(
                    replacement,
                    fixture
                        .string_units(&format!("{prefix}/expected/replacement"))
                        .unwrap(),
                    "case {index}"
                );
            }
        }
    }

    #[test]
    fn native_322_matching_and_replacement_representation() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/file_mutation_322.json"
        ))
        .unwrap();
        for case in fixture["edits"].as_array().unwrap() {
            let file = case["file"].as_str().unwrap();
            let old = case["old"].as_str().unwrap();
            let actual = find_actual_string(file, old);
            let expected = &case["expected"];
            assert_eq!(
                actual.as_deref(),
                expected["actual"].as_str(),
                "{}",
                case["name"]
            );
            if let Some(actual) = actual {
                let replacement = preserve_escape_style(
                    old,
                    &actual,
                    &preserve_quote_style(old, &actual, case["replacement"].as_str().unwrap()),
                );
                assert_eq!(
                    replacement,
                    expected["replacement"].as_str().unwrap(),
                    "{}",
                    case["name"]
                );
            }
        }
    }

    #[test]
    fn normalize_quotes_maps_curly_to_straight() {
        let input = format!(
            "{LEFT_DOUBLE_CURLY_QUOTE}hi{RIGHT_DOUBLE_CURLY_QUOTE} {LEFT_SINGLE_CURLY_QUOTE}yo{RIGHT_SINGLE_CURLY_QUOTE}"
        );
        assert_eq!(normalize_quotes(&input), "\"hi\" 'yo'");
    }

    #[test]
    fn normalize_quotes_leaves_straight_untouched() {
        assert_eq!(normalize_quotes("\"hi\" 'yo'"), "\"hi\" 'yo'");
    }

    #[test]
    fn find_actual_string_exact_match() {
        assert_eq!(
            find_actual_string("hello world", "world"),
            Some("world".to_string())
        );
    }

    #[test]
    fn find_actual_string_recovers_curly_double() {
        // File has curly double quotes, model sent straight double quotes.
        let file = format!("say {LEFT_DOUBLE_CURLY_QUOTE}hello{RIGHT_DOUBLE_CURLY_QUOTE} now");
        let actual = find_actual_string(&file, "\"hello\"").unwrap();
        assert_eq!(
            actual,
            format!("{LEFT_DOUBLE_CURLY_QUOTE}hello{RIGHT_DOUBLE_CURLY_QUOTE}")
        );
    }

    #[test]
    fn find_actual_string_recovers_curly_single() {
        let file = format!("a {LEFT_SINGLE_CURLY_QUOTE}b{RIGHT_SINGLE_CURLY_QUOTE} c");
        let actual = find_actual_string(&file, "'b'").unwrap();
        assert_eq!(
            actual,
            format!("{LEFT_SINGLE_CURLY_QUOTE}b{RIGHT_SINGLE_CURLY_QUOTE}")
        );
    }

    #[test]
    fn curly_recovery_preserves_character_windows_after_unicode_prefixes() {
        for (file, search, expected) in [
            ("pré😀 say “hello” now", "\"hello\"", "“hello”"),
            ("前 ‘名字’ suffix", "'名字'", "‘名字’"),
            ("“prefix” 😀 say “世界”", "\"世界\"", "“世界”"),
            ("😀 “😀” suffix", "\"😀\"", "“😀”"),
        ] {
            assert_eq!(find_actual_string(file, search).as_deref(), Some(expected));
        }
    }

    #[test]
    fn find_actual_string_none_when_absent() {
        assert_eq!(find_actual_string("hello world", "absent"), None);
    }

    // ----- `vIe` step (3): `\uXXXX` escape-swap fallback (Tlo + flo) -----

    #[test]
    fn find_actual_string_decodes_escape_to_literal() {
        // File has the literal char `é`; old_string was sent as the `é`
        // escape. Steps 1+2 miss (no literal `é` in file, curly-normalize
        // is a no-op), step 3 decodes `é`→`é` and finds it.
        let file = "let name = café;";
        let actual = find_actual_string(file, "caf\\u00e9").unwrap();
        assert_eq!(actual, "café");
    }

    #[test]
    fn find_actual_string_escape_swap_preserves_doubled_backslash() {
        // `flo` leaves a literal doubled backslash alone: `\\u0041` must NOT be
        // decoded to `A` (the `\\` is consumed as a literal pair first), so the
        // string is unchanged and no decoded match is attempted. The file does
        // not contain the raw text, so the lookup fails.
        let file = "plain A text";
        // old contains `\\` then `u0041` — decode_unicode_escapes keeps it as-is
        // so `s == search`, the `s != search` guard fails ⇒ no decode match.
        assert_eq!(find_actual_string(file, "\\\\u0041"), None);
    }

    #[test]
    fn decode_unicode_escapes_basic_and_doubled() {
        // `é` → `é`; doubled backslash preserved verbatim.
        assert_eq!(decode_unicode_escapes("caf\\u00e9"), "café");
        assert_eq!(decode_unicode_escapes("a\\\\u0041b"), "a\\\\u0041b");
        // A real escape after non-escape text decodes; lone `\u` w/o 4 hex stays.
        assert_eq!(decode_unicode_escapes("x\\u41"), "x\\u41");
    }

    // ----- `vIe` step (4): non-ASCII escaped-form regex fallback (Slo + dUa) --

    #[test]
    fn find_actual_string_matches_escaped_form_of_literal_non_ascii() {
        // File stores the ESCAPED form `é`; old_string has the literal `é`.
        // Steps 1+2+3 miss (no literal `é` / `\uXXXX` in old), step 4 builds the
        // escaped pattern `\u00[eE]9` and finds it in the file.
        let file = "json: \\u00e9 end";
        let actual = find_actual_string(file, "é").unwrap();
        assert_eq!(actual, "\\u00e9");
    }

    #[test]
    fn find_actual_string_escaped_form_case_insensitive_hex() {
        // dUa makes a-f hex case-insensitive: literal `é` matches `é`.
        let file = "x \\u00E9 y";
        let actual = find_actual_string(file, "é").unwrap();
        assert_eq!(actual, "\\u00E9");
    }

    #[test]
    fn find_actual_string_escaped_form_mixed_ascii_and_non_ascii() {
        // `café` literal: `c`,`a`,`f` literal, `é` as `é` escaped form.
        let file = "prefix caf\\u00e9 suffix";
        let actual = find_actual_string(file, "café").unwrap();
        assert_eq!(actual, "caf\\u00e9");
    }

    #[test]
    fn find_actual_string_terminal_none_with_non_ascii() {
        // Non-ASCII old_string whose escaped form is absent ⇒ all four miss.
        assert_eq!(find_actual_string("nothing here", "é"), None);
    }

    // ----- predicates: Tlo / Slo / pUa -----

    #[test]
    fn contains_unicode_escape_predicate() {
        assert!(contains_unicode_escape("a\\u00e9b"));
        assert!(contains_unicode_escape("\\uFFFF"));
        assert!(!contains_unicode_escape("\\u00g9")); // non-hex
        assert!(!contains_unicode_escape("plain text"));
        assert!(!contains_unicode_escape("\\u123")); // only 3 hex
    }

    #[test]
    fn contains_non_ascii_bmp_predicate() {
        assert!(contains_non_ascii_bmp("café"));
        assert!(contains_non_ascii_bmp("\u{2018}")); // curly quote is BMP non-ASCII
        assert!(!contains_non_ascii_bmp("plain ascii"));
        // Astral-plane (> U+FFFF) does NOT satisfy Slo.
        assert!(contains_non_ascii_bmp("\u{1F600}"));
    }

    #[test]
    fn has_escape_or_non_ascii_predicate() {
        // pUa = Tlo || Slo.
        assert!(has_escape_or_non_ascii("\\u00e9")); // escape only
        assert!(has_escape_or_non_ascii("é")); // non-ascii only
        assert!(has_escape_or_non_ascii("café \\uFFFF")); // both
        assert!(!has_escape_or_non_ascii("plain ascii string"));
    }

    #[test]
    fn preserve_quote_style_noop_when_same() {
        // old == actual_old ⇒ no normalization happened ⇒ new untouched.
        assert_eq!(preserve_quote_style("foo", "foo", "\"bar\""), "\"bar\"");
    }

    #[test]
    fn preserve_quote_style_double_open_close_context() {
        // File used curly doubles, so new straight doubles become curly by
        // open/close context.
        let actual_old = format!("{LEFT_DOUBLE_CURLY_QUOTE}x{RIGHT_DOUBLE_CURLY_QUOTE}");
        let out = preserve_quote_style("\"x\"", &actual_old, "say \"hi\" ok");
        assert_eq!(
            out,
            format!("say {LEFT_DOUBLE_CURLY_QUOTE}hi{RIGHT_DOUBLE_CURLY_QUOTE} ok")
        );
    }

    #[test]
    fn preserve_quote_style_contraction_keeps_right_single() {
        // `don't` — apostrophe between two letters is a contraction, not an
        // opening quote ⇒ right single curly.
        let actual_old = format!("{LEFT_SINGLE_CURLY_QUOTE}x{RIGHT_SINGLE_CURLY_QUOTE}");
        let out = preserve_quote_style("'x'", &actual_old, "don't");
        assert_eq!(out, format!("don{RIGHT_SINGLE_CURLY_QUOTE}t"));
    }

    #[test]
    fn preserve_quote_style_single_open_vs_close_positions() {
        let actual_old = format!("{LEFT_SINGLE_CURLY_QUOTE}x{RIGHT_SINGLE_CURLY_QUOTE}");
        // Leading quote (start of string) ⇒ opening; trailing quote (after a
        // letter, no letter after) ⇒ closing.
        let out = preserve_quote_style("'x'", &actual_old, "'word'");
        assert_eq!(
            out,
            format!("{LEFT_SINGLE_CURLY_QUOTE}word{RIGHT_SINGLE_CURLY_QUOTE}")
        );
    }

    #[test]
    fn preserve_quote_style_no_curly_in_file_leaves_new_untouched() {
        // actual_old differs from old but contains no curly quotes ⇒ unchanged.
        let out = preserve_quote_style("abc", "abd", "\"new\"");
        assert_eq!(out, "\"new\"");
    }
}
