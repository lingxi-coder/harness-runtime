//! ANSI / SGR parser producing [`StyledLine`]s.
//!
//! Moved+expanded from M6's `src/ansi.rs`. M6 handled 8/16-color SGR +
//! reset/bold only. M7-01 adds 256-color (`38;5;N` / `48;5;N`), truecolor
//! (`38;2;r;g;b` / `48;2;r;g;b`), italic/underline attributes, and safely
//! skips cursor-move / erase CSI sequences (CUU/CUD/CUF/CUB/ED/EL) without
//! corrupting surrounding text. (256/truecolor + cursor-skip land in the
//! following tasks; this file first re-establishes M6 parity.)
//!
//! Operates on `&str` (UTF-8 already decoded). Output splits on `\n` into
//! one [`StyledLine`] per visual line. Never panics on malformed input.

use crate::presentation::render::{NamedColor, SpanStyle, StyleColor, StyledLine, StyledSpan};

/// Parse `input` into styled lines. Unsupported escape sequences are
/// silently skipped. Never panics.
#[must_use]
pub fn parse_ansi(input: &str) -> Vec<StyledLine> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<StyledLine> = Vec::new();
    let mut current: Vec<StyledSpan> = Vec::new();
    let mut style = SpanStyle::default();
    let bytes = input.as_bytes();
    let mut i = 0;
    // Start byte index of the current text run. Text bytes are accumulated by
    // index (not pushed byte-by-byte) so the run is sliced out of the original
    // `&str` as a UTF-8 string. Escape sequences only ever start/end on ASCII
    // byte boundaries, so `start` and `i` always land on char boundaries.
    let mut run_start = 0usize;

    // Flush the pending text run `[run_start, end)` into `current` as a span
    // with the active style, decoding it as UTF-8 (never byte-as-char).
    macro_rules! flush_run {
        ($end:expr) => {{
            let end = $end;
            if end > run_start {
                current.push(StyledSpan::styled(&input[run_start..end], style));
            }
        }};
    }

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            flush_run!(i);
            lines.push(StyledLine {
                spans: coalesce(std::mem::take(&mut current)),
            });
            i += 1;
            run_start = i;
            continue;
        }
        if b == 0x1b && i + 1 < bytes.len() {
            flush_run!(i);
            let next = bytes[i + 1];
            if next == b'[' {
                // CSI — read params until a final byte in 0x40..=0x7E.
                let mut j = i + 2;
                let mut params = String::new();
                while j < bytes.len() {
                    let c = bytes[j];
                    if (0x40..=0x7E).contains(&c) {
                        break;
                    }
                    params.push(c as char);
                    j += 1;
                }
                if j >= bytes.len() {
                    // Unterminated CSI — drop it. The pre-ESC text was already
                    // flushed above; mark the run consumed so the post-loop
                    // flush does not re-emit the dropped escape bytes as text.
                    run_start = bytes.len();
                    break;
                }
                let final_byte = bytes[j];
                if final_byte == b'm' {
                    apply_sgr(&params, &mut style);
                }
                // Else: non-SGR CSI (cursor/erase/mode) — skipped (Task 5
                // makes the skip explicit + tested).
                i = j + 1;
                run_start = i;
                continue;
            } else if next == b']' {
                // OSC — read to BEL (0x07) or ST (ESC \).
                let mut j = i + 2;
                while j < bytes.len() {
                    if bytes[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
                run_start = i;
                continue;
            }
            // Other ESC-prefixed sequence (ESC c, ESC =, …). The byte after ESC
            // is an ASCII intermediate/final; consume it too. But if it is
            // non-ASCII it is the start of a real char (not part of the escape)
            // — skip only the ESC byte so run_start stays on a UTF-8 char
            // boundary (this fn must never panic). The `i + 1` access is safe:
            // this branch is guarded by `i + 1 < bytes.len()` above.
            if bytes[i + 1] < 0x80 {
                i += 2;
            } else {
                i += 1;
            }
            run_start = i;
            continue;
        }
        // Text byte (ASCII or part of a multi-byte UTF-8 sequence) — leave it
        // in the run; it is decoded when the run is flushed.
        i += 1;
    }

    flush_run!(bytes.len());
    if !current.is_empty() {
        lines.push(StyledLine {
            spans: coalesce(current),
        });
    }
    lines
}

/// Merge adjacent spans with identical style + kind so skipped escape
/// sequences (which split a run) collapse back into one span.
fn coalesce(spans: Vec<StyledSpan>) -> Vec<StyledSpan> {
    let mut out: Vec<StyledSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        match out.last_mut() {
            Some(prev) if prev.style == span.style && prev.kind == span.kind => {
                prev.text.push_str(&span.text);
            }
            _ => out.push(span),
        }
    }
    out
}

/// Apply a semicolon-separated SGR parameter string to `style`. Walks the
/// parameter list by index so the extended `38`/`48` color introducers can
/// pull their `5;N` (indexed) or `2;r;g;b` (truecolor) sub-parameters.
/// Empty params reset. Unsupported / truncated codes are ignored without
/// panicking.
fn apply_sgr(params: &str, style: &mut SpanStyle) {
    if params.is_empty() {
        *style = SpanStyle::default();
        return;
    }
    let parts: Vec<u16> = params.split(';').map(|t| t.parse().unwrap_or(0)).collect();
    let mut idx = 0;
    while idx < parts.len() {
        let n = parts[idx];
        match n {
            0 => *style = SpanStyle::default(),
            1 => style.bold = true,
            3 => style.italic = true,
            4 => style.underline = true,
            22 => style.bold = false,
            23 => style.italic = false,
            24 => style.underline = false,
            30..=37 => style.fg = StyleColor::Named(named_from_offset(n - 30)),
            39 => style.fg = StyleColor::Default,
            40..=47 => style.bg = StyleColor::Named(named_from_offset(n - 40)),
            49 => style.bg = StyleColor::Default,
            90..=97 => style.fg = StyleColor::Named(bright_from_offset(n - 90)),
            100..=107 => style.bg = StyleColor::Named(bright_from_offset(n - 100)),
            38 => {
                if let Some(color) = parse_extended_color(&parts, &mut idx) {
                    style.fg = color;
                }
            }
            48 => {
                if let Some(color) = parse_extended_color(&parts, &mut idx) {
                    style.bg = color;
                }
            }
            _ => { /* unsupported code — ignore */ }
        }
        idx += 1;
    }
}

/// Parse the sub-parameters of a `38`/`48` introducer. `idx` points at the
/// introducer (`38`/`48`); on success it is advanced past the consumed
/// sub-parameters. Returns `None` (consuming nothing extra) on a truncated
/// or unrecognized form.
///
/// SGR color sub-parameters are defined in 0..=255; the `as u8` casts narrow
/// the parsed `u16` to that byte range (out-of-range values wrap, matching
/// terminal leniency).
#[allow(clippy::cast_possible_truncation)]
fn parse_extended_color(parts: &[u16], idx: &mut usize) -> Option<StyleColor> {
    match parts.get(*idx + 1) {
        Some(5) => {
            let n = *parts.get(*idx + 2)?;
            *idx += 2;
            Some(StyleColor::Indexed(n as u8))
        }
        Some(2) => {
            let r = *parts.get(*idx + 2)?;
            let g = *parts.get(*idx + 3)?;
            let b = *parts.get(*idx + 4)?;
            *idx += 4;
            Some(StyleColor::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

/// Map an offset 0..=7 to the standard named color (SGR 30..=37 / 40..=47).
fn named_from_offset(off: u16) -> NamedColor {
    match off {
        0 => NamedColor::Black,
        1 => NamedColor::Red,
        2 => NamedColor::Green,
        3 => NamedColor::Yellow,
        4 => NamedColor::Blue,
        5 => NamedColor::Magenta,
        6 => NamedColor::Cyan,
        _ => NamedColor::White,
    }
}

/// Map an offset 0..=7 to the bright named color (SGR 90..=97 / 100..=107).
fn bright_from_offset(off: u16) -> NamedColor {
    match off {
        0 => NamedColor::BrightBlack,
        1 => NamedColor::BrightRed,
        2 => NamedColor::BrightGreen,
        3 => NamedColor::BrightYellow,
        4 => NamedColor::BrightBlue,
        5 => NamedColor::BrightMagenta,
        6 => NamedColor::BrightCyan,
        _ => NamedColor::BrightWhite,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::render::{NamedColor, SpanKind, StyleColor};

    fn first_line(input: &str) -> StyledLine {
        parse_ansi(input).into_iter().next().unwrap_or_default()
    }

    #[test]
    fn red_err_then_reset() {
        let line = first_line("\x1b[31mERR\x1b[0m");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "ERR");
        assert_eq!(line.spans[0].style.fg, StyleColor::Named(NamedColor::Red));
    }

    #[test]
    fn multi_param_sgr_bold_red() {
        let line = first_line("\x1b[1;31mhi\x1b[0m");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "hi");
        assert_eq!(line.spans[0].style.fg, StyleColor::Named(NamedColor::Red));
        assert!(line.spans[0].style.bold);
    }

    #[test]
    fn empty_string_parses_to_empty_vec() {
        assert!(parse_ansi("").is_empty());
    }

    #[test]
    fn newline_splits_into_lines() {
        let lines = parse_ansi("a\nb");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].plain_text(), "a");
        assert_eq!(lines[1].plain_text(), "b");
    }

    #[test]
    fn all_spans_are_text_kind() {
        let line = first_line("\x1b[31mERR\x1b[0m");
        assert_eq!(line.spans[0].kind, SpanKind::Text);
    }

    #[test]
    fn malformed_unterminated_csi_does_not_panic() {
        let _ = parse_ansi("\x1b[31");
    }

    #[test]
    fn fg_256_color_indexed() {
        // 38;5;196 = bright red in the 256 palette.
        let line = first_line("\x1b[38;5;196mX\x1b[0m");
        assert_eq!(line.spans[0].text, "X");
        assert_eq!(line.spans[0].style.fg, StyleColor::Indexed(196));
    }

    #[test]
    fn bg_256_color_indexed() {
        let line = first_line("\x1b[48;5;21mX\x1b[0m");
        assert_eq!(line.spans[0].style.bg, StyleColor::Indexed(21));
    }

    #[test]
    fn fg_truecolor_rgb() {
        let line = first_line("\x1b[38;2;10;20;30mX\x1b[0m");
        assert_eq!(line.spans[0].style.fg, StyleColor::Rgb(10, 20, 30));
    }

    #[test]
    fn bg_truecolor_rgb() {
        let line = first_line("\x1b[48;2;200;100;50mX\x1b[0m");
        assert_eq!(line.spans[0].style.bg, StyleColor::Rgb(200, 100, 50));
    }

    #[test]
    fn truecolor_mixed_with_bold() {
        // bold + truecolor fg in one SGR.
        let line = first_line("\x1b[1;38;2;1;2;3mX\x1b[0m");
        assert!(line.spans[0].style.bold);
        assert_eq!(line.spans[0].style.fg, StyleColor::Rgb(1, 2, 3));
    }

    #[test]
    fn truncated_256_sequence_does_not_panic() {
        // 38;5 with no index — must not panic, must not corrupt.
        let _ = first_line("\x1b[38;5mtail");
    }

    #[test]
    fn cursor_up_is_skipped_without_corruption() {
        // CUU (cursor up) between two letters — text must remain "ab".
        let line = first_line("a\x1b[3Ab");
        assert_eq!(line.plain_text(), "ab");
        assert_eq!(line.spans.len(), 1);
    }

    #[test]
    fn cursor_forward_back_skipped() {
        let line = first_line("a\x1b[2Cb\x1b[1Dc");
        assert_eq!(line.plain_text(), "abc");
    }

    #[test]
    fn erase_display_skipped() {
        let line = first_line("x\x1b[2Jy");
        assert_eq!(line.plain_text(), "xy");
    }

    #[test]
    fn erase_line_skipped() {
        let line = first_line("x\x1b[Ky");
        assert_eq!(line.plain_text(), "xy");
    }

    #[test]
    fn cursor_skip_preserves_surrounding_color() {
        // Color set, cursor move, then text — color must still apply.
        let line = first_line("\x1b[31m\x1b[2Aerr\x1b[0m");
        assert_eq!(line.plain_text(), "err");
        assert_eq!(line.spans[0].style.fg, StyleColor::Named(NamedColor::Red));
    }

    #[test]
    fn osc_title_skipped() {
        let line = first_line("a\x1b]0;title\x07b");
        assert_eq!(line.plain_text(), "ab");
    }

    #[test]
    fn multibyte_plain_text_roundtrips() {
        // Accented Latin must decode as UTF-8, not Latin-1 (no mojibake).
        let line = first_line("café");
        assert_eq!(line.plain_text(), "café");
    }

    #[test]
    fn multibyte_text_with_sgr_keeps_style_and_text() {
        // Red "café" then reset — span text exact, style red.
        let line = first_line("\x1b[31mcafé\x1b[0m");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].text, "café");
        assert_eq!(line.spans[0].style.fg, StyleColor::Named(NamedColor::Red));
    }

    #[test]
    fn cjk_text_roundtrips_byte_for_byte() {
        let line = first_line("日本語");
        assert_eq!(line.plain_text(), "日本語");
    }

    #[test]
    fn box_drawing_text_roundtrips_byte_for_byte() {
        let line = first_line("│─┐");
        assert_eq!(line.plain_text(), "│─┐");
    }

    #[test]
    fn emoji_text_roundtrips() {
        let line = first_line("✓ 🚀 done");
        assert_eq!(line.plain_text(), "✓ 🚀 done");
    }

    #[test]
    fn esc_then_multibyte_char_does_not_panic() {
        // Bare ESC followed by a 2-byte UTF-8 char ('é'). The ESC has no
        // recognized CSI/OSC second byte; the multibyte char is a real char,
        // not part of the escape, and must survive intact (no mid-char slice).
        let line = first_line("\u{1b}é");
        assert!(line.plain_text().contains('é'));
    }

    #[test]
    fn esc_then_multibyte_char_then_ascii_does_not_panic() {
        let line = first_line("\u{1b}éx");
        let plain = line.plain_text();
        assert!(plain.contains('é'));
        assert!(plain.contains('x'));
    }

    #[test]
    fn esc_then_emoji_does_not_panic() {
        // 4-byte UTF-8 lead byte after ESC must not be mis-skipped.
        let line = first_line("\u{1b}🚀tail");
        let plain = line.plain_text();
        assert!(plain.contains('🚀'));
        assert!(plain.contains("tail"));
    }

    #[test]
    fn trailing_lone_esc_does_not_panic() {
        let line = first_line("ab\u{1b}");
        assert!(line.plain_text().contains("ab"));
    }

    #[test]
    fn snapshot_16_color() {
        insta::assert_yaml_snapshot!(parse_ansi("\x1b[31mERR\x1b[0m\x1b[1mB\x1b[0m tail"));
    }

    #[test]
    fn snapshot_256_color() {
        insta::assert_yaml_snapshot!(parse_ansi("\x1b[38;5;196mhot\x1b[48;5;21m bg\x1b[0m"));
    }

    #[test]
    fn snapshot_truecolor() {
        insta::assert_yaml_snapshot!(parse_ansi("\x1b[38;2;255;128;0morange\x1b[0m"));
    }

    #[test]
    fn snapshot_reset_midline() {
        insta::assert_yaml_snapshot!(parse_ansi("\x1b[1;32mok\x1b[0m done"));
    }

    #[test]
    fn snapshot_malformed_unterminated() {
        insta::assert_yaml_snapshot!(parse_ansi("before\x1b[38;5"));
    }

    #[test]
    fn snapshot_cursor_move_skipped() {
        insta::assert_yaml_snapshot!(parse_ansi("\x1b[31ma\x1b[2A\x1b[2Jb\x1b[0m"));
    }
}
