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
//! ## 1:1 fidelity / divergence note
//!
//! Byte-faithful for BMP text and the contraction heuristic. The one **close**
//! (not byte-faithful) edge: TS slices the recovered substring with
//! `fileContent.substring(idx, idx + searchString.length)` where JS `.length`
//! is a count of **UTF-16 code units**. Rust here uses **char** counts. For
//! astral-plane chars (emoji, etc.) in `old_string`, a single char is one Rust
//! char but two UTF-16 code units, so the recovered window length differs from
//! TS. This only affects `old_string`s containing astral chars whose match
//! relied on quote normalization — pure-BMP text (the overwhelmingly common
//! case, and all curly-quote text) is byte-faithful.

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
/// Uses char iteration (not bytes). See the module-doc divergence note for the
/// astral-plane edge versus TS UTF-16 `.length`.
#[must_use]
pub fn find_actual_string(file: &str, search: &str) -> Option<String> {
    // (1) First try exact match.
    if file.contains(search) {
        return Some(search.to_string());
    }

    // (2) Try with normalized quotes (the curly-quote recovery).
    let normalized_search = normalize_quotes(search);
    let normalized_file = normalize_quotes(file);

    // `normalizeQuotes` maps each curly quote to exactly one straight quote, so
    // char indices in the normalized file align 1:1 with the original file's
    // char indices. Find the search by char index in the normalized file, then
    // slice the original file's chars by the same window.
    let normalized_file_chars: Vec<char> = normalized_file.chars().collect();
    let normalized_search_chars: Vec<char> = normalized_search.chars().collect();
    if let Some(search_char_index) = char_index_of(&normalized_file_chars, &normalized_search_chars)
    {
        // TS: fileContent.substring(searchIndex, searchIndex + searchString.length).
        // We use the ORIGINAL search's char length (JS `.length` is UTF-16 units;
        // see module divergence note — BMP-faithful).
        let window_len = search.chars().count();
        let original_chars: Vec<char> = file.chars().collect();
        let end = (search_char_index + window_len).min(original_chars.len());
        return Some(original_chars[search_char_index..end].iter().collect());
    }

    // (3) Escape-swap fallback (`vIe` step 3 / `Tlo` + `flo`): if `search`
    // contains a `\uXXXX` escape sequence, decode those escapes to their
    // characters (preserving any literal doubled `\\`). If decoding changed the
    // string AND the (decoded) form appears verbatim in the file, that decoded
    // form is the actual string.
    if contains_unicode_escape(search) {
        let decoded = decode_unicode_escapes(search);
        if decoded != search && file.contains(&decoded) {
            return Some(decoded);
        }
    }

    // (4) Non-ASCII regex fallback (`vIe` step 4 / `Slo` + `dUa`): if `search`
    // contains any non-ASCII char (U+0080..U+FFFF), build a matcher that finds
    // the **escaped** `\uXXXX` representation of each non-ASCII codepoint
    // (case-insensitive on a-f hex digits) and the literal of each ASCII char,
    // then return the first matching substring of the file.
    if contains_non_ascii_bmp(search) {
        if let Some(m) = match_escaped_form(file, search) {
            return Some(m);
        }
    }

    None
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

/// `Slo.test(e)` where `Slo=/[-￿]/` — true if `e` contains any char
/// in the range U+0080..=U+FFFF (a non-ASCII Basic-Multilingual-Plane char).
/// Astral-plane chars (> U+FFFF) do NOT satisfy `Slo` (the binary's regex range
/// caps at U+FFFF), so they are excluded here for fidelity.
#[must_use]
pub fn contains_non_ascii_bmp(s: &str) -> bool {
    s.chars().any(|c| ('\u{0080}'..='\u{ffff}').contains(&c))
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
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        // Alternative 1: a literal doubled backslash `\\` — copy verbatim,
        // consuming both backslashes so a following `uXXXX` is left literal.
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '\\' {
            out.push('\\');
            out.push('\\');
            i += 2;
            continue;
        }
        // Alternative 2: `\uXXXX` — decode the 4 hex digits to a char.
        if chars[i] == '\\'
            && i + 6 <= chars.len()
            && chars[i + 1] == 'u'
            && chars[i + 2..i + 6].iter().all(char::is_ascii_hexdigit)
        {
            let hex: String = chars[i + 2..i + 6].iter().collect();
            // `String.fromCharCode(parseInt(hex,16))` — a single UTF-16 code
            // unit. For the BMP this is a valid scalar value; a lone surrogate
            // (D800..DFFF) is not a Rust char, so fall back to copying the raw
            // escape text unchanged (faithful in practice: surrogate escapes in
            // an `old_string` would not match a real file char either).
            if let Some(code) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(code);
                i += 6;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Build the per-char escaped-or-literal pattern of `dUa` (offset 201326362) and
/// find the FIRST substring of `file` that matches it, returning that substring.
///
/// ```text
/// function dUa(e){let t="";for(let n=0;n<e.length;n++){let r=e.charCodeAt(n);
///   if(r>=128){t+="\\u";for(let o of r.toString(16).padStart(4,"0"))
///     t+=o>="a"?`[${o}${o.toUpperCase()}]`:o}else t+=VI(e[n])}return t}
/// ```
///
/// For a non-ASCII codepoint the pattern matches its literal `\uXXXX` escaped
/// text in the file (case-insensitive on the a-f hex digits); for an ASCII char
/// it matches that char literally. Returns the matched run (the captured `s[0]`
/// of `e.match(new RegExp(dUa(t)))`), or `None`.
fn match_escaped_form(file: &str, search: &str) -> Option<String> {
    // Compile `search` into a sequence of single-char matchers.
    let pattern: Vec<PatternUnit> = search.chars().map(PatternUnit::for_char).collect();
    if pattern.is_empty() {
        // `new RegExp("")` matches the empty string at position 0; `s[0]` == "".
        return Some(String::new());
    }
    let file_chars: Vec<char> = file.chars().collect();
    // Try every start position (regex `.match` finds the leftmost match).
    for start in 0..=file_chars.len() {
        if let Some(end) = try_match_at(&file_chars, start, &pattern) {
            return Some(file_chars[start..end].iter().collect());
        }
    }
    None
}

/// One unit of a `dUa` pattern: either a literal ASCII char, or a non-ASCII
/// codepoint that matches its `\uXXXX` escaped text (case-insensitive hex).
enum PatternUnit {
    /// Match this exact char (the ASCII / `VI(e[n])` literal branch).
    Literal(char),
    /// Match the 6-char escape `\u` + 4 hex digits of this codepoint
    /// (case-insensitive on a-f), i.e. the `r>=128` branch of `dUa`.
    Escaped { hex: [char; 4] },
}

impl PatternUnit {
    fn for_char(c: char) -> Self {
        let code = c as u32;
        if code >= 128 {
            // `r.toString(16).padStart(4,"0")` — but `charCodeAt` yields UTF-16
            // code units (<= 0xFFFF for the BMP). Non-ASCII BMP chars are the
            // only ones that reach here from `Slo`; an astral char never sets
            // `Slo`, so `find_actual_string` won't invoke this path for it. For
            // robustness, low-16-bit hex matches `charCodeAt` for the BMP.
            let lower = format!("{:04x}", code & 0xffff);
            let mut hex = ['0'; 4];
            for (slot, ch) in hex.iter_mut().zip(lower.chars()) {
                *slot = ch;
            }
            PatternUnit::Escaped { hex }
        } else {
            PatternUnit::Literal(c)
        }
    }

    /// Number of file chars this unit consumes when it matches.
    fn width(&self) -> usize {
        match self {
            PatternUnit::Literal(_) => 1,
            PatternUnit::Escaped { .. } => 6, // `\u` + 4 hex digits
        }
    }

    /// Does this unit match the run of `chars` starting at `at`?
    fn matches_at(&self, chars: &[char], at: usize) -> bool {
        match self {
            PatternUnit::Literal(c) => chars.get(at) == Some(c),
            PatternUnit::Escaped { hex } => {
                if at + 6 > chars.len() {
                    return false;
                }
                chars[at] == '\\'
                    && chars[at + 1] == 'u'
                    && hex
                        .iter()
                        .zip(&chars[at + 2..at + 6])
                        // `[${o}${o.toUpperCase()}]` — match either case of the
                        // hex digit (only a-f differ in case).
                        .all(|(want, got)| got.eq_ignore_ascii_case(want))
            }
        }
    }
}

/// Try to match the full `pattern` against `file_chars` anchored at `start`.
/// Returns the end index (exclusive) on success.
fn try_match_at(file_chars: &[char], start: usize, pattern: &[PatternUnit]) -> Option<usize> {
    let mut pos = start;
    for unit in pattern {
        if !unit.matches_at(file_chars, pos) {
            return None;
        }
        pos += unit.width();
    }
    Some(pos)
}

/// Index of the first occurrence of `needle` within `haystack` (char slices),
/// or `None`. Mirrors JS `String.prototype.indexOf` over normalized content.
fn char_index_of(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|&start| haystack[start..start + needle.len()] == *needle)
}

/// When `old_string` matched via quote normalization (curly quotes in file,
/// straight quotes from model), apply the same curly quote style to `new_string`
/// so the edit preserves the file's typography. Port of `preserveQuoteStyle`
/// (`utils.ts:104-136`).
#[must_use]
pub fn preserve_quote_style(old_string: &str, actual_old_string: &str, new_string: &str) -> String {
    // If they're the same, no normalization happened.
    if old_string == actual_old_string {
        return new_string.to_string();
    }

    // Detect which curly quote types were in the file.
    let has_double_quotes = actual_old_string.contains(LEFT_DOUBLE_CURLY_QUOTE)
        || actual_old_string.contains(RIGHT_DOUBLE_CURLY_QUOTE);
    let has_single_quotes = actual_old_string.contains(LEFT_SINGLE_CURLY_QUOTE)
        || actual_old_string.contains(RIGHT_SINGLE_CURLY_QUOTE);

    if !has_double_quotes && !has_single_quotes {
        return new_string.to_string();
    }

    let mut result = new_string.to_string();

    if has_double_quotes {
        result = apply_curly_double_quotes(&result);
    }
    if has_single_quotes {
        result = apply_curly_single_quotes(&result);
    }

    result
}

/// A quote character preceded by whitespace, start of string, or opening
/// punctuation is treated as an opening quote. Port of `isOpeningContext`
/// (`utils.ts:138-154`).
fn is_opening_context(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let prev = chars[index - 1];
    matches!(
        prev,
        ' ' | '\t'
            | '\n'
            | '\r'
            | '('
            | '['
            | '{'
            | '\u{2014}' // em dash
            | '\u{2013}' // en dash
    )
}

/// Replace straight double quotes with open/close curly doubles. Port of
/// `applyCurlyDoubleQuotes` (`utils.ts:156-171`).
fn apply_curly_double_quotes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '"' {
            result.push(if is_opening_context(&chars, i) {
                LEFT_DOUBLE_CURLY_QUOTE
            } else {
                RIGHT_DOUBLE_CURLY_QUOTE
            });
        } else {
            result.push(c);
        }
    }
    result
}

/// Replace straight single quotes with open/close curly singles, treating an
/// apostrophe between two letters as a contraction (→ right single curly). Port
/// of `applyCurlySingleQuotes` (`utils.ts:173-199`).
fn apply_curly_single_quotes(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len());
    for i in 0..chars.len() {
        if chars[i] == '\'' {
            // Don't convert apostrophes in contractions (e.g., "don't", "it's").
            // An apostrophe between two letters is a contraction, not a quote.
            let prev_is_letter = i > 0 && chars[i - 1].is_alphabetic();
            let next_is_letter = i + 1 < chars.len() && chars[i + 1].is_alphabetic();
            if prev_is_letter && next_is_letter {
                // Apostrophe in a contraction — use right single curly quote.
                result.push(RIGHT_SINGLE_CURLY_QUOTE);
            } else {
                result.push(if is_opening_context(&chars, i) {
                    LEFT_SINGLE_CURLY_QUOTE
                } else {
                    RIGHT_SINGLE_CURLY_QUOTE
                });
            }
        } else {
            result.push(chars[i]);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!contains_non_ascii_bmp("\u{1F600}"));
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
