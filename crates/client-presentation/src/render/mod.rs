//! Shared rendering primitives for the TUI surface (M7).
//!
//! This module owns the styled-line value model that every render
//! submodule and downstream message renderer speaks:
//!   - [`StyleColor`] — supersedes M6's 16-color `AnsiColor` with 256-color
//!     (`Indexed`) and truecolor (`Rgb`) support.
//!   - [`SpanStyle`] — fg/bg + bold/italic/underline attributes.
//!   - [`StyledSpan`] — one styled run; [`SpanKind`] tags code-fence
//!     placeholders that M7-02 fills with syntax-highlighted spans.
//!   - [`StyledLine`] — a single visual line (no embedded `\n`).
//!
//! `render::ansi` and `render::markdown` both produce `Vec<StyledLine>`.

pub mod ansi;
pub mod diff;
pub mod markdown;
pub mod markdown_table;
pub mod model_name;
pub mod osc8;
pub mod syntax;

/// The 16 named SGR colors (8 standard + 8 bright). Carried over from M6's
/// `AnsiColor`; lives inside [`StyleColor::Named`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum NamedColor {
    /// SGR 30 / 40.
    Black,
    /// SGR 31 / 41.
    Red,
    /// SGR 32 / 42.
    Green,
    /// SGR 33 / 43.
    Yellow,
    /// SGR 34 / 44.
    Blue,
    /// SGR 35 / 45.
    Magenta,
    /// SGR 36 / 46.
    Cyan,
    /// SGR 37 / 47.
    White,
    /// SGR 90 / 100.
    BrightBlack,
    /// SGR 91 / 101.
    BrightRed,
    /// SGR 92 / 102.
    BrightGreen,
    /// SGR 93 / 103.
    BrightYellow,
    /// SGR 94 / 104.
    BrightBlue,
    /// SGR 95 / 105.
    BrightMagenta,
    /// SGR 96 / 106.
    BrightCyan,
    /// SGR 97 / 107.
    BrightWhite,
}

/// A foreground or background color slot. Supersedes M6's `AnsiColor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum StyleColor {
    /// Terminal default — no override.
    #[default]
    Default,
    /// One of the 16 named SGR colors.
    Named(NamedColor),
    /// 256-color palette index (`38;5;N` / `48;5;N`).
    Indexed(u8),
    /// 24-bit truecolor (`38;2;r;g;b` / `48;2;r;g;b`).
    Rgb(u8, u8, u8),
}

/// Map a claude-code agent **color name** (e.g. `"cyan"`, `"orange"`) to a
/// neutral [`StyleColor`]. Case-insensitive. The 6 ANSI hues use `Named`; the
/// other 4 use `Rgb` (equivalent-look parity with the iocraft `agent_color`,
/// not byte-identical). Unknown / empty names fall back to cyan — claude-code's
/// `cyan_FOR_SUBAGENTS_ONLY` default.
#[must_use]
pub fn agent_color_from_name(name: &str) -> StyleColor {
    match name.to_ascii_lowercase().as_str() {
        "magenta" => StyleColor::Named(NamedColor::Magenta),
        "yellow" => StyleColor::Named(NamedColor::Yellow),
        "green" => StyleColor::Named(NamedColor::Green),
        "blue" => StyleColor::Named(NamedColor::Blue),
        "red" => StyleColor::Named(NamedColor::Red),
        "orange" => StyleColor::Rgb(255, 165, 0),
        "purple" => StyleColor::Rgb(160, 90, 220),
        "pink" => StyleColor::Rgb(255, 130, 180),
        "teal" => StyleColor::Rgb(0, 160, 160),
        // "cyan" and anything unknown → cyan.
        _ => StyleColor::Named(NamedColor::Cyan),
    }
}

/// Resolve an xterm 256-color palette index to an 8-bit RGB triple.
/// 0..=15 are the system colors, 16..=231 the 6×6×6 cube, 232..=255 the
/// grayscale ramp. (Standard xterm mapping.)
#[allow(clippy::many_single_char_names)]
pub fn xterm256_to_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0 => (0, 0, 0),
        1 => (128, 0, 0),
        2 => (0, 128, 0),
        3 => (128, 128, 0),
        4 => (0, 0, 128),
        5 => (128, 0, 128),
        6 => (0, 128, 128),
        7 => (192, 192, 192),
        8 => (128, 128, 128),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (0, 0, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        16..=231 => {
            let c = i - 16;
            let r = c / 36;
            let g = (c % 36) / 6;
            let b = c % 6;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(r), level(g), level(b))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Visual attributes applied to a [`StyledSpan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct SpanStyle {
    /// Foreground color.
    pub fg: StyleColor,
    /// Background color.
    pub bg: StyleColor,
    /// Bold weight (SGR 1 / 22).
    pub bold: bool,
    /// Italic (SGR 3 / 23; markdown `em`).
    pub italic: bool,
    /// Underline (SGR 4 / 24; markdown h1).
    pub underline: bool,
}

/// What kind of content a span carries. Most spans are plain [`Text`].
/// [`CodePlaceholder`] marks a fenced code block that M7-02's syntect pass
/// replaces with highlighted spans — M7-01 never highlights.
///
/// [`Text`]: SpanKind::Text
/// [`CodePlaceholder`]: SpanKind::CodePlaceholder
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum SpanKind {
    /// Ordinary styled text.
    Text,
    /// Raw fenced-code content awaiting M7-02 highlighting. `lang` is the
    /// fence info-string (e.g. `rust`) when present.
    CodePlaceholder {
        /// Language hint from the fence info-string, if any.
        lang: Option<String>,
    },
}

/// One styled run of text. `text` never contains a newline — lines are
/// split into separate [`StyledLine`] values.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StyledSpan {
    /// UTF-8 text content of the run.
    pub text: String,
    /// Active style for this run.
    pub style: SpanStyle,
    /// Content classification.
    pub kind: SpanKind,
}

impl StyledSpan {
    /// A default-styled plain-text span.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        StyledSpan {
            text: text.into(),
            style: SpanStyle::default(),
            kind: SpanKind::Text,
        }
    }

    /// A styled plain-text span.
    #[must_use]
    pub fn styled(text: impl Into<String>, style: SpanStyle) -> Self {
        StyledSpan {
            text: text.into(),
            style,
            kind: SpanKind::Text,
        }
    }

    /// A code-fence placeholder span carrying the raw block text + lang hint.
    #[must_use]
    pub fn code_placeholder(text: impl Into<String>, lang: Option<&str>) -> Self {
        StyledSpan {
            text: text.into(),
            style: SpanStyle::default(),
            kind: SpanKind::CodePlaceholder {
                lang: lang.map(str::to_string),
            },
        }
    }
}

/// A single visual line: an ordered list of styled spans, no embedded `\n`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct StyledLine {
    /// The spans composing this line, left to right.
    pub spans: Vec<StyledSpan>,
}

impl StyledLine {
    /// An empty line (used for spacing between block elements).
    #[must_use]
    pub fn empty() -> Self {
        StyledLine { spans: Vec::new() }
    }

    /// A line with a single default-styled span.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        StyledLine {
            spans: vec![StyledSpan::plain(text)],
        }
    }

    /// Concatenate every span's text (style-stripped). Useful for tests and
    /// width math.
    #[must_use]
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// Split a flat span stream into per-visual-line groups, consuming the
/// `\n`-only delimiter spans that [`render::ansi`](crate::render::ansi)-style
/// flatteners insert between parsed lines.
///
/// `render_bash_output_spans` / `render_user_tool_result_body_spans` produce a
/// single `Vec<StyledSpan>` where parsed lines are rejoined with plain `"\n"`
/// spans. A flex **Row** of those children lays everything out horizontally —
/// collapsing N lines onto one visual row. Renderers that want one row per line
/// call this helper to recover the per-line span groups, then wrap each group
/// in its own row inside a `FlexDirection::Column`.
///
/// Behavior:
///   - Splits at every span whose `text` is exactly `"\n"` (the delimiter the
///     flatteners emit). Those delimiter spans are consumed, not rendered.
///   - A span carrying an embedded `\n` (not a pure delimiter) is split on its
///     newlines too, so the result never contains an embedded newline.
///   - An empty input yields an empty `Vec` (no spurious blank line) so callers
///     reproduce their prior empty-body behavior. A single-line input yields a
///     single group (one row — no behavior change for the common case).
#[must_use]
pub fn split_spans_into_line_rows(spans: Vec<StyledSpan>) -> Vec<Vec<StyledSpan>> {
    let mut lines: Vec<Vec<StyledSpan>> = Vec::new();
    let mut current: Vec<StyledSpan> = Vec::new();
    let mut started = false;

    for span in spans {
        // Pure newline delimiter span → close the current line.
        if span.text == "\n" {
            lines.push(std::mem::take(&mut current));
            started = true;
            continue;
        }
        // Defensive: a span may carry embedded newlines (e.g. a placeholder).
        // Split it so no rendered span ever contains a `\n`.
        if span.text.contains('\n') {
            let mut parts = span.text.split('\n').peekable();
            while let Some(part) = parts.next() {
                if !part.is_empty() {
                    current.push(StyledSpan {
                        text: part.to_string(),
                        style: span.style,
                        kind: span.kind.clone(),
                    });
                }
                if parts.peek().is_some() {
                    lines.push(std::mem::take(&mut current));
                    started = true;
                }
            }
            continue;
        }
        started = true;
        current.push(span);
    }

    if started || !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Single-line trailing-ellipsis truncation (claude-code `truncateToWidth`,
/// `utils/truncate.ts`): display-width aware (`unicode-width`, so wide CJK
/// glyphs count double), splits on grapheme clusters (no chopping a
/// multi-codepoint emoji in half). `max_width <= 1` collapses to a bare `…`.
#[must_use]
pub fn truncate_to_width_ellipsis(text: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "\u{2026}".to_string();
    }
    let mut width = 0usize;
    let mut result = String::new();
    for seg in text.graphemes(true) {
        let seg_width = UnicodeWidthStr::width(seg);
        if width + seg_width > max_width - 1 {
            break;
        }
        result.push_str(seg);
        width += seg_width;
    }
    result.push('\u{2026}');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_to_width_ellipsis_passes_short_text_through() {
        assert_eq!(truncate_to_width_ellipsis("hi", 10), "hi");
    }

    #[test]
    fn agent_color_from_name_maps_known_and_falls_back_to_cyan() {
        assert_eq!(
            agent_color_from_name("magenta"),
            StyleColor::Named(NamedColor::Magenta)
        );
        assert_eq!(
            agent_color_from_name("Orange"),
            StyleColor::Rgb(255, 165, 0)
        );
        assert_eq!(agent_color_from_name("teal"), StyleColor::Rgb(0, 160, 160));
        // Unknown / empty → cyan (claude-code cyan_FOR_SUBAGENTS_ONLY default).
        assert_eq!(
            agent_color_from_name("chartreuse"),
            StyleColor::Named(NamedColor::Cyan)
        );
        assert_eq!(
            agent_color_from_name(""),
            StyleColor::Named(NamedColor::Cyan)
        );
    }

    #[test]
    fn truncate_to_width_ellipsis_truncates_long_text() {
        assert_eq!(
            truncate_to_width_ellipsis("hello world", 6),
            "hello\u{2026}"
        );
    }

    #[test]
    fn truncate_to_width_ellipsis_max_width_one_is_bare_ellipsis() {
        assert_eq!(truncate_to_width_ellipsis("hello", 1), "\u{2026}");
    }

    #[test]
    fn styled_line_holds_spans() {
        let line = StyledLine {
            spans: vec![
                StyledSpan::plain("hello "),
                StyledSpan {
                    text: "world".to_string(),
                    style: SpanStyle {
                        fg: StyleColor::Named(NamedColor::Red),
                        bold: true,
                        ..SpanStyle::default()
                    },
                    kind: SpanKind::Text,
                },
            ],
        };
        assert_eq!(line.spans.len(), 2);
        assert_eq!(line.plain_text(), "hello world");
        assert_eq!(line.spans[1].style.fg, StyleColor::Named(NamedColor::Red));
    }

    #[test]
    fn code_placeholder_span_is_tagged() {
        let span = StyledSpan::code_placeholder("fn main() {}", Some("rust"));
        assert_eq!(
            span.kind,
            SpanKind::CodePlaceholder {
                lang: Some("rust".to_string())
            }
        );
        assert_eq!(span.text, "fn main() {}");
    }

    #[test]
    fn default_style_is_plain_default() {
        let s = SpanStyle::default();
        assert_eq!(s.fg, StyleColor::Default);
        assert_eq!(s.bg, StyleColor::Default);
        assert!(!s.bold);
        assert!(!s.italic);
        assert!(!s.underline);
    }

    #[test]
    fn split_spans_empty_yields_no_lines() {
        assert!(split_spans_into_line_rows(Vec::new()).is_empty());
    }

    #[test]
    fn split_spans_single_line_one_group() {
        let rows = split_spans_into_line_rows(vec![
            StyledSpan::plain("hello "),
            StyledSpan::plain("world"),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2);
        let joined: String = rows[0].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, "hello world");
    }

    #[test]
    fn split_spans_splits_at_newline_delimiters() {
        // "a" \n "b" \n "c" — delimiter spans consumed, three line groups.
        let rows = split_spans_into_line_rows(vec![
            StyledSpan::plain("a"),
            StyledSpan::plain("\n"),
            StyledSpan::plain("b"),
            StyledSpan::plain("\n"),
            StyledSpan::plain("c"),
        ]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0].text, "a");
        assert_eq!(rows[1][0].text, "b");
        assert_eq!(rows[2][0].text, "c");
        // No rendered span carries a newline.
        assert!(rows.iter().flatten().all(|s| !s.text.contains('\n')));
    }

    #[test]
    fn split_spans_preserves_per_span_style() {
        let red = SpanStyle {
            fg: StyleColor::Named(NamedColor::Red),
            ..SpanStyle::default()
        };
        let rows = split_spans_into_line_rows(vec![
            StyledSpan::styled("err", red),
            StyledSpan::plain("\n"),
            StyledSpan::plain("ok"),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].style.fg, StyleColor::Named(NamedColor::Red));
        assert_eq!(rows[1][0].style.fg, StyleColor::Default);
    }

    #[test]
    fn split_spans_handles_embedded_newline_in_span() {
        // A single span carrying an embedded `\n` is split too.
        let rows = split_spans_into_line_rows(vec![StyledSpan::plain("a\nb")]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].text, "a");
        assert_eq!(rows[1][0].text, "b");
    }
}
