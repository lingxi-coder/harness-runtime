//! `w1` — the display sanitizer claude-code runs over every identifier it
//! interpolates into a task error message.
//!
//! Verbatim from `src_157227596.js` @99533:
//!
//! ```js
//! function w1(e,t=160){
//!   let o=Rae(e)
//!     .replace(/[\p{Cc}\p{Cf}]/gu,(r)=>/\s/.test(r)?r:"")
//!     .replace(/\s+/g," ")
//!     .trim();
//!   return o.length>t?`${oe(o,t)}…`:o
//! }
//! ```
//!
//! It is not cosmetic. Task ids and agent names reach these messages from the
//! model and from other agents, so an unsanitized one can carry bidi overrides
//! or zero-width characters into the transcript.
//!
//! Three of the four helpers are reproduced here; the fourth is a no-op:
//!
//! - `Rae` (`src_156484250.js` @ the `Rae` definition) strips LONE SURROGATES.
//!   A Rust `&str` is always well-formed UTF-8 and cannot hold one, so it has
//!   nothing to do.
//! - `oe(t,n)` (`src_156484250.js` @954) slices to `n` UTF-16 code units and
//!   drops a trailing HIGH surrogate so a pair is never split. Counting
//!   `char::len_utf16` per char reproduces both halves at once: a `char` is
//!   whole by construction, so the boundary can never land inside a pair.
//! - The length test is `o.length > t` — JS string length, i.e. UTF-16 CODE
//!   UNITS, not bytes and not chars. An emoji counts 2.

/// `t = 160`, the default every call site in the task cluster uses.
pub const DISPLAY_LIMIT: usize = 160;

/// The JS `\s` class, which is what decides whether a `Cc`/`Cf` character
/// survives the strip. Note U+FEFF is in it (and is also `Cf`), so a BOM
/// survives the strip and is then collapsed into a plain space.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Unicode general category `Cf` (format). `Cc` is covered by
/// [`char::is_control`], which is exactly U+0000–U+001F plus U+007F–U+009F.
///
/// Spelled out rather than pulled from a properties crate: this is the only
/// place in the workspace that needs the category, and the table is short
/// enough to read. Every range is from Unicode 15.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// `w1(e)` with the oracle's default limit.
#[must_use]
pub fn sanitize_display(value: &str) -> String {
    sanitize_display_to(value, DISPLAY_LIMIT)
}

/// `w1(e, t)`.
#[must_use]
pub fn sanitize_display_to(value: &str, limit: usize) -> String {
    // `.replace(/[\p{Cc}\p{Cf}]/gu, r => /\s/.test(r) ? r : "")` — a control or
    // format char survives ONLY when JS would call it whitespace.
    let stripped: String = value
        .chars()
        .filter(|c| !(c.is_control() || is_format_char(*c)) || is_js_whitespace(*c))
        .collect();

    // `.replace(/\s+/g, " ").trim()`.
    let mut collapsed = String::with_capacity(stripped.len());
    let mut in_space = false;
    for c in stripped.chars() {
        if is_js_whitespace(c) {
            in_space = true;
            continue;
        }
        if in_space && !collapsed.is_empty() {
            collapsed.push(' ');
        }
        in_space = false;
        collapsed.push(c);
    }

    // `o.length > t ? `${oe(o,t)}…` : o` — UTF-16 code units on both sides.
    let utf16_len: usize = collapsed.chars().map(char::len_utf16).sum();
    if utf16_len <= limit {
        return collapsed;
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in collapsed.chars() {
        let width = c.len_utf16();
        if used + width > limit {
            break;
        }
        used += width;
        out.push(c);
    }
    out.push('\u{2026}');
    out
}

/// Unicode binary property `Variation_Selector` (Unicode 15): the Mongolian
/// free variation selectors, the BMP block, and the supplementary block.
fn is_variation_selector(c: char) -> bool {
    matches!(
        c,
        '\u{180B}'..='\u{180D}'
            | '\u{180F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

/// `\p{Zl}` | `\p{Zp}` — the two separator characters the MCP display
/// sanitizers name explicitly (they are NOT in `Cc`/`Cf`).
fn is_line_or_paragraph_separator(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// The classes `Ge` (in `rg`) strips, minus `\p{Cc}`: format characters, the
/// two separators, and variation selectors. `\p{Cs}` is unreachable — a Rust
/// `&str` cannot hold a lone surrogate.
///
/// Public because the MCP `statusMessage` normalizer (`v`, in the task
/// notification renderer) strips the SAME set plus `\p{Cc}`, and two
/// hand-copied tables would be free to drift apart.
#[must_use]
pub fn is_mcp_stripped_class(c: char) -> bool {
    is_format_char(c) || is_line_or_paragraph_separator(c) || is_variation_selector(c)
}

/// `oe(t, n)` (`src_156484250.js` @954) — a surrogate-safe UTF-16 prefix.
///
/// The oracle slices to `n` code units and drops a trailing HIGH surrogate so a
/// pair is never split. Accumulating `char::len_utf16` reproduces that: a Rust
/// `char` is whole by construction, so the boundary can never land inside a
/// pair. No ellipsis is appended — `oe` is a plain cut.
fn truncate_utf16_units(value: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut used = 0usize;
    let mut out = String::new();
    for c in value.chars() {
        let width = c.len_utf16();
        if used + width > limit {
            break;
        }
        used += width;
        out.push(c);
    }
    out
}

/// `ESn = 128` — the intermediate cap inside `e1e`.
const MCP_ID_INTERMEDIATE_UNITS: usize = 128;

/// The `rG` cap: an `mcpTaskId` is shown as its first 8 UTF-16 units.
pub const MCP_TASK_ID_UNITS: usize = 8;

/// `rG(r)` (`src_160977784.js` @883) — the short form of an MCP task id:
///
/// ```js
/// var ESn = 128;
/// function e1e(r){return oe(r.replace(/[\p{Cc}\p{Cf}\p{Cs}\p{Zl}\p{Zp}\p{Variation_Selector}]+/gu,""),ESn)}
/// function rG(r){return oe(e1e(r),8)}
/// ```
///
/// Note what this is NOT: the characters are DELETED, not replaced with a
/// space (that is [`sanitize_mcp_name`]'s rule), and NO ellipsis is appended —
/// a long id is simply cut to its first 8 units. `\p{Cs}` is unreachable here
/// because a Rust `&str` cannot hold a lone surrogate.
///
/// The 128-unit intermediate cut is kept even though the 8-unit cut subsumes
/// it: it is one composed function upstream, and a future change to either
/// bound should not have to re-derive the other.
#[must_use]
pub fn sanitize_mcp_task_id(value: &str) -> String {
    let stripped: String = value
        .chars()
        .filter(|c| !(c.is_control() || is_mcp_stripped_class(*c)))
        .collect();
    let intermediate = truncate_utf16_units(&stripped, MCP_ID_INTERMEDIATE_UNITS);
    truncate_utf16_units(&intermediate, MCP_TASK_ID_UNITS)
}

/// `ie = 200` — the display-column cap in `rg`.
pub const MCP_NAME_WIDTH: usize = 200;

/// `Ye = ie * 4 = 800` — the UTF-16 pre-cut in `rg`, which bounds the work the
/// width pass has to do.
pub const MCP_NAME_UNITS: usize = MCP_NAME_WIDTH * 4;

/// `rg(e)` (`src_160860334.js` @31193) — the display sanitizer claude-code runs
/// over an MCP server or tool name before interpolating it:
///
/// ```js
/// var ie=200, Ye=ie*4, Ge=/[\p{Cf}\p{Cs}\p{Zl}\p{Zp}\p{Variation_Selector}]+/gu;
/// function rg(e){let r=e===void 0?"":To(pt(e).replace(Ge," "));
///   return r===""?void 0:Xe(oe(r,Ye),ie)}
/// ```
///
/// `Ge` REPLACES each run with a single space (unlike
/// [`sanitize_mcp_task_id`], which deletes), `To` collapses whitespace runs and
/// trims, an empty result becomes `None`, and `Xe` is a grapheme-segmented
/// truncation to 200 display COLUMNS with a trailing `…`.
///
/// ANSI sequences are stripped before invisible-character and whitespace
/// normalization, matching `Bun.stripANSI` even for names from configuration.
#[must_use]
pub fn sanitize_mcp_name(value: &str) -> Option<String> {
    // `.replace(Ge, " ")` — each RUN becomes one space.
    let mut replaced = String::with_capacity(value.len());
    let mut in_run = false;
    for c in strip_ansi_text(value).chars() {
        if is_mcp_stripped_class(c) {
            if !in_run {
                replaced.push(' ');
                in_run = true;
            }
            continue;
        }
        in_run = false;
        replaced.push(c);
    }

    // `To(t) = t.replace(<ansi>, "").replace(/\s+/g, " ").trim()`.
    let mut collapsed = String::with_capacity(replaced.len());
    let mut pending_space = false;
    for c in replaced.chars() {
        if is_js_whitespace(c) {
            pending_space = true;
            continue;
        }
        if pending_space && !collapsed.is_empty() {
            collapsed.push(' ');
        }
        pending_space = false;
        collapsed.push(c);
    }

    if collapsed.is_empty() {
        return None;
    }
    Some(truncate_to_width_ellipsis(
        &truncate_utf16_units(&collapsed, MCP_NAME_UNITS),
        MCP_NAME_WIDTH,
    ))
}

/// `Xe(t, e)` (`src_157736630.js` @7307) — grapheme-segmented truncation to `e`
/// display COLUMNS:
///
/// ```js
/// function Xe(t,e){if(te(t)<=e)return t;if(e<=1)return"\u2026";
///   let n=0,r="";for(let{segment:o}of Xs().segment(t)){let i=te(o);
///     if(n+i>e-1)break;r+=o,n+=i}return r+"\u2026"}
/// ```
///
/// `te` is `Bun.stringWidth` and `Xs()` an `Intl.Segmenter` over graphemes; the
/// port spells those as `unicode-width` and `unicode-segmentation`, the same
/// pairing every other `Xe`-family port in the workspace uses. The budget for
/// the kept segments is `e - 1`, leaving a column for the ellipsis.
fn truncate_to_width_ellipsis(value: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation as _;
    use unicode_width::UnicodeWidthStr as _;

    if value.width() <= max_width {
        return value.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    let budget = max_width - 1;
    let mut used = 0usize;
    let mut out = String::new();
    for segment in value.graphemes(true) {
        let width = segment.width();
        if used + width > budget {
            break;
        }
        out.push_str(segment);
        used += width;
    }
    out.push('\u{2026}');
    out
}

/// Strip CSI, OSC and two-byte terminal escape sequences without dependencies.
/// UTF-8 text outside a complete escape sequence is preserved.
pub fn strip_ansi_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        let marker = if c == '\u{1b}' {
            chars.next()
        } else if c == '\u{9b}' {
            Some('[')
        } else if c == '\u{9d}' {
            Some(']')
        } else {
            out.push(c);
            continue;
        };
        match marker {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' || next == '\u{9c}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(' '..='/') => {
                while chars.peek().is_some_and(|c| (' '..='/').contains(c)) {
                    chars.next();
                }
                chars.next();
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rG` DELETES the stripped classes and appends no ellipsis — the two ways
    /// it differs from every other sanitizer in this module.
    #[test]
    fn an_mcp_task_id_is_cut_to_eight_units_with_no_ellipsis() {
        assert_eq!(sanitize_mcp_task_id("k1234567890abcdef"), "k1234567");
        assert_eq!(sanitize_mcp_task_id("short"), "short");
        // U+200B (Cf) and U+2028 (Zl) are removed, not replaced — the surviving
        // characters close up, so eight of them still fit.
        assert_eq!(
            sanitize_mcp_task_id("a\u{200b}b\u{2028}cdefghij"),
            "abcdefgh"
        );
        // A variation selector is removed too.
        assert_eq!(sanitize_mcp_task_id("a\u{fe0f}bc"), "abc");
    }

    /// An astral character costs TWO UTF-16 units, so seven of them plus an
    /// emoji cannot fit: the cut lands before the emoji rather than splitting
    /// its surrogate pair.
    #[test]
    fn an_mcp_task_id_never_splits_a_surrogate_pair() {
        assert_eq!(sanitize_mcp_task_id("abcdefg\u{1f600}"), "abcdefg");
        assert_eq!(sanitize_mcp_task_id("abcdef\u{1f600}"), "abcdef\u{1f600}");
    }

    /// `rg` REPLACES each stripped run with ONE space, then collapses and
    /// trims — so a name made only of stripped characters becomes `None`, which
    /// is what makes `Pee` render an empty side.
    #[test]
    fn an_mcp_name_replaces_runs_with_one_space_and_empties_to_none() {
        assert_eq!(sanitize_mcp_name("github"), Some("github".to_string()));
        assert_eq!(
            sanitize_mcp_name("git\u{200b}\u{200c}hub"),
            Some("git hub".to_string()),
            "a RUN of format chars becomes exactly one space"
        );
        assert_eq!(
            sanitize_mcp_name("  spaced   out  "),
            Some("spaced out".to_string())
        );
        assert_eq!(sanitize_mcp_name(""), None);
        assert_eq!(sanitize_mcp_name("\u{200b}\u{2028}"), None);
    }

    /// `Xe`'s budget is `e - 1`, leaving one column for the ellipsis, and it
    /// measures DISPLAY WIDTH over graphemes — a wide CJK character costs two.
    #[test]
    fn an_over_long_mcp_name_is_cut_to_display_columns_with_an_ellipsis() {
        let long = "a".repeat(250);
        let out = sanitize_mcp_name(&long).expect("non-empty");
        assert_eq!(out, format!("{}\u{2026}", "a".repeat(MCP_NAME_WIDTH - 1)));

        // 150 wide characters = 300 columns. The budget is 199 columns, so 99
        // of them fit (198) and the 100th would overshoot.
        let wide = "\u{4e2d}".repeat(150);
        let out = sanitize_mcp_name(&wide).expect("non-empty");
        assert_eq!(out, format!("{}\u{2026}", "\u{4e2d}".repeat(99)));
    }

    /// A name exactly at the cap keeps every column and gains no ellipsis.
    #[test]
    fn an_mcp_name_at_the_cap_is_untouched() {
        let exact = "a".repeat(MCP_NAME_WIDTH);
        assert_eq!(sanitize_mcp_name(&exact), Some(exact));
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(sanitize_display("buddy@alpha"), "buddy@alpha");
        assert_eq!(sanitize_display(""), "");
    }

    /// The reason this exists: an id carrying a bidi override or a zero-width
    /// character must not reach the transcript intact.
    #[test]
    fn control_and_format_characters_are_stripped() {
        // U+202E RIGHT-TO-LEFT OVERRIDE (Cf, not JS whitespace).
        assert_eq!(sanitize_display("a\u{202E}b"), "ab");
        // U+200B ZERO WIDTH SPACE (Cf, NOT in the JS `\s` class).
        assert_eq!(sanitize_display("a\u{200B}b"), "ab");
        // A C0 control (Cc).
        assert_eq!(sanitize_display("a\u{0007}b"), "ab");
        // U+00AD SOFT HYPHEN (Cf).
        assert_eq!(sanitize_display("a\u{00AD}b"), "ab");
    }

    /// U+FEFF is BOTH `Cf` and JS whitespace, so the strip keeps it and the
    /// collapse turns it into a plain space. Getting the guard backwards would
    /// delete it instead.
    #[test]
    fn a_bom_becomes_a_space_rather_than_vanishing() {
        assert_eq!(sanitize_display("a\u{FEFF}b"), "a b");
    }

    #[test]
    fn whitespace_is_collapsed_and_trimmed() {
        assert_eq!(sanitize_display("  a \t\n  b  "), "a b");
        assert_eq!(sanitize_display("\u{3000}a\u{2028}b\u{00A0}"), "a b");
        assert_eq!(sanitize_display("   "), "");
    }

    /// The limit is UTF-16 code units, not chars and not bytes — an emoji
    /// counts 2. A char-counting port would let 160 emoji (320 code units)
    /// through untruncated.
    #[test]
    fn the_limit_counts_utf16_code_units() {
        let ascii = "a".repeat(160);
        assert_eq!(sanitize_display(&ascii), ascii, "exactly at the limit");

        let over = "a".repeat(161);
        let out = sanitize_display(&over);
        assert_eq!(out.chars().count(), 161, "160 kept plus the ellipsis");
        assert!(out.ends_with('\u{2026}'));

        // 81 astral chars = 162 UTF-16 units ⇒ over the limit.
        let astral = "😀".repeat(81);
        let out = sanitize_display(&astral);
        assert!(out.ends_with('\u{2026}'));
        let kept: usize = out
            .chars()
            .filter(|c| *c != '\u{2026}')
            .map(char::len_utf16)
            .sum();
        assert_eq!(kept, 160, "the cut lands on a whole char, never mid-pair");
    }

    /// `oe`'s high-surrogate guard: the truncation boundary must never split a
    /// pair, which for chars means the last kept char is whole.
    #[test]
    fn truncation_never_splits_an_astral_char() {
        // 159 ASCII + one astral char = 161 units; the astral char cannot fit
        // in the remaining single unit, so it is dropped whole.
        let value = format!("{}{}", "a".repeat(159), "😀");
        let out = sanitize_display(&value);
        assert_eq!(out, format!("{}\u{2026}", "a".repeat(159)));
    }
}
