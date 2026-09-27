//! syntect syntax highlighting wrapper (M7-02).
//!
//! Parity (design §0 Q3): equivalent-look highlighting, NOT byte-identical to
//! highlight.js. Tests assert which spans are colored, not exact colors.
//!
//! M7-01 type note: `StyledSpan` is the nested shape
//! `{ text, style: SpanStyle { fg, bg, bold, .. }, kind }`, NOT a flat
//! `{ text, fg, bg, bold }`. A syntect `Color { r, g, b, a }` maps to
//! `StyleColor::Rgb(r, g, b)` (alpha dropped); `FontStyle::BOLD` → `style.bold`.

// `highlight.js`, `syntect`, `base16-ocean.dark` etc. read better unquoted in
// the module prose; suppress the doc-markdown nudge crate-wide for this file
// (matches `render/markdown.rs`).
#![allow(clippy::doc_markdown)]

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color as SynColor, FontStyle, Style as SynStyle, Theme as SynTheme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::render::{SpanStyle, StyleColor, StyledLine, StyledSpan};
use crate::theme::ThemeName;

/// Theme-independent semantic class of a highlighted run.
///
/// This is the *portable* half of syntax highlighting. [`highlight`] resolves
/// syntect scopes all the way down to concrete RGB against one bundled
/// `.tmTheme`, which is exactly right for the terminal and exactly wrong for a
/// client that has its own palette and a light mode — a dark-theme syntect
/// color painted on a light background is unreadable.
///
/// So the wire carries the class and each surface picks its own color.
/// [`classify_line`] derives these from the TextMate scope stack; the terminal
/// keeps using the resolved RGB and is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum SyntaxClass {
    /// No classification — render in the surface's default code color.
    #[default]
    Plain,
    /// Language keywords and storage modifiers (`fn`, `let`, `pub`, `async`).
    Keyword,
    /// Type / class / trait / enum / tag names.
    TypeName,
    /// Function and method names, at definition and call sites.
    Function,
    /// String and character literals.
    StringLit,
    /// Numeric literals.
    Number,
    /// Comments and doc comments.
    Comment,
    /// Brackets, delimiters, and separators.
    Punctuation,
    /// Operators.
    Operator,
    /// Variable and parameter names.
    Variable,
    /// Named constants (`true`, `None`, language builtins).
    Constant,
    /// Attributes / annotations / decorators.
    Attribute,
}

/// TextMate scope-name prefix → [`SyntaxClass`]. Order matters: the first
/// matching prefix wins, so more specific prefixes precede their parents
/// (`constant.numeric` before `constant`, `keyword.operator` before `keyword`).
const SCOPE_CLASS_TABLE: &[(&str, SyntaxClass)] = &[
    ("comment", SyntaxClass::Comment),
    ("string", SyntaxClass::StringLit),
    ("constant.numeric", SyntaxClass::Number),
    ("constant.character", SyntaxClass::StringLit),
    ("constant", SyntaxClass::Constant),
    ("keyword.operator", SyntaxClass::Operator),
    ("keyword", SyntaxClass::Keyword),
    ("storage", SyntaxClass::Keyword),
    ("entity.name.function", SyntaxClass::Function),
    ("entity.name.type", SyntaxClass::TypeName),
    ("entity.name.class", SyntaxClass::TypeName),
    ("entity.name.struct", SyntaxClass::TypeName),
    ("entity.name.enum", SyntaxClass::TypeName),
    ("entity.name.trait", SyntaxClass::TypeName),
    ("entity.name.namespace", SyntaxClass::TypeName),
    ("entity.name.tag", SyntaxClass::TypeName),
    ("entity.other.attribute-name", SyntaxClass::Attribute),
    ("support.function", SyntaxClass::Function),
    ("support.type", SyntaxClass::TypeName),
    ("support.class", SyntaxClass::TypeName),
    ("support.constant", SyntaxClass::Constant),
    ("meta.function-call", SyntaxClass::Function),
    ("meta.annotation", SyntaxClass::Attribute),
    ("variable.parameter", SyntaxClass::Variable),
    ("variable", SyntaxClass::Variable),
    ("punctuation", SyntaxClass::Punctuation),
];

/// Classify one scope stack by its most specific matching scope.
///
/// `punctuation.definition.*` is skipped on the first pass: TextMate scopes a
/// comment's `//` and a string's quotes as punctuation *nested inside* the
/// construct they delimit, so a naive most-specific-first walk would paint the
/// `//` of a comment as [`SyntaxClass::Punctuation`] and only the text after it
/// as [`SyntaxClass::Comment`]. Delimiters should read as part of what they
/// delimit, so the walk looks past them for the enclosing construct and only
/// falls back to punctuation if there is nothing else.
fn classify_stack(stack: &ScopeStack) -> SyntaxClass {
    let matched = |name: &str| {
        SCOPE_CLASS_TABLE
            .iter()
            .find(|(prefix, _)| name.starts_with(prefix))
            .map(|(_, class)| *class)
    };
    // Top of the stack is the most specific scope; walk outward.
    let names: Vec<String> = stack
        .as_slice()
        .iter()
        .rev()
        .map(|scope| scope.build_string())
        .collect();
    for name in &names {
        if name.starts_with("punctuation.definition.") {
            continue;
        }
        if let Some(class) = matched(name) {
            return class;
        }
    }
    for name in &names {
        if let Some(class) = matched(name) {
            return class;
        }
    }
    SyntaxClass::Plain
}

/// Semantic classes for one line, as half-open UTF-8 **byte** ranges into
/// `line`, in order and non-overlapping.
///
/// Deliberately independent of [`highlight`]: it re-parses the line rather
/// than re-splitting the highlighter's spans, so attaching classes can never
/// move a terminal span boundary. Callers look up the class covering a span's
/// start offset. Unknown language, or any parse error, yields an empty vec —
/// treat every run as [`SyntaxClass::Plain`].
///
/// `line` is one line WITHOUT a trailing newline; scope state is not carried
/// across lines, so a multi-line construct (a block comment, a raw string)
/// classifies only by what is visible on the line. That is acceptable here
/// because the caller is a diff, which shows lines out of context anyway.
#[must_use]
pub fn classify_line(line: &str, lang: Option<&str>) -> Vec<(std::ops::Range<usize>, SyntaxClass)> {
    if line.is_empty() {
        return Vec::new();
    }
    let ss = syntax_set();
    let Some(syntax) = lang.and_then(|l| {
        ss.find_syntax_by_token(l)
            .or_else(|| ss.find_syntax_by_extension(l))
    }) else {
        return Vec::new();
    };
    let mut state = ParseState::new(syntax);
    // The bundled syntaxes are `load_defaults_newlines`, so the parser expects
    // a trailing newline; feed one and clamp offsets back to `line`.
    let owned = format!("{line}\n");
    let Ok(ops) = state.parse_line(&owned, ss) else {
        return Vec::new();
    };
    let mut stack = ScopeStack::new();
    let mut out: Vec<(std::ops::Range<usize>, SyntaxClass)> = Vec::new();
    let mut prev = 0usize;
    for (offset, op) in &ops {
        let offset = (*offset).min(line.len());
        if offset > prev {
            out.push((prev..offset, classify_stack(&stack)));
        }
        if stack.apply(op).is_err() {
            return out;
        }
        prev = prev.max(offset);
    }
    if prev < line.len() {
        out.push((prev..line.len(), classify_stack(&stack)));
    }
    out
}

/// The class covering byte offset `at` in a [`classify_line`] result.
#[must_use]
pub fn class_at(classes: &[(std::ops::Range<usize>, SyntaxClass)], at: usize) -> SyntaxClass {
    classes
        .iter()
        .find(|(range, _)| range.contains(&at))
        .map_or(SyntaxClass::Plain, |(_, class)| *class)
}

fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static TS: OnceLock<ThemeSet> = OnceLock::new();
    TS.get_or_init(ThemeSet::load_defaults)
}

/// (M7-15) Map the active [`ThemeName`] to its bundled syntect `.tmTheme`
/// NAME. Dark themes (incl. dark-daltonized + dark-ansi) → a dark tmTheme;
/// light themes → a light one. ANSI themes reuse the dark/light mapping (the
/// terminal handles the ANSI palette). The returned name MUST exist in
/// `ThemeSet::load_defaults().themes` (default set ships base16-ocean.dark,
/// base16-ocean.light, base16-eighties.dark, base16-mocha.dark,
/// InspiredGitHub, Solarized (dark), Solarized (light)).
#[must_use]
fn tm_theme_name(name: ThemeName) -> &'static str {
    match name {
        ThemeName::Dark | ThemeName::DarkDaltonized | ThemeName::DarkAnsi => "base16-ocean.dark",
        ThemeName::Light | ThemeName::LightDaltonized | ThemeName::LightAnsi => {
            "base16-ocean.light"
        }
    }
}

/// (M7-15) Pick the bundled syntect `.tmTheme` for the active TUI theme.
/// Lazy-loaded via the shared `ThemeSet` (§4 R9 — no extra `.tmTheme` files;
/// the default set is enough). Falls back to the dark theme if the mapped name
/// is somehow absent (cannot happen with the bundled set, but avoids a panic).
#[must_use]
pub fn tm_theme_for(name: ThemeName) -> &'static SynTheme {
    let ts = theme_set();
    let key = tm_theme_name(name);
    ts.themes
        .get(key)
        .unwrap_or_else(|| &ts.themes["base16-ocean.dark"])
}

/// Resolve a language token from a fence info-string (preferred), a file
/// path's filename/extension, or (syntax-01) a shebang/first-line heuristic.
/// Returns a token suitable for syntect's `find_syntax_by_token` /
/// `find_syntax_by_extension`. `None` → plain.
#[must_use]
pub fn detect_language(info_string: Option<&str>, path: Option<&str>) -> Option<String> {
    detect_language_with_first_line(info_string, path, None)
}

/// [`detect_language`] plus a (syntax-01) shebang/first-line fallback,
/// consulted only when neither the info-string nor the filename/extension
/// resolved a language — claude-code `detectLanguage`'s final branch.
#[must_use]
pub fn detect_language_with_first_line(
    info_string: Option<&str>,
    path: Option<&str>,
    first_line: Option<&str>,
) -> Option<String> {
    // Fence info-string wins. CommonMark info-strings may carry metadata
    // after the language (e.g. "rust,ignore" or "ts {1,3}"); take the first
    // whitespace/comma-delimited token.
    if let Some(info) = info_string {
        let token = info
            .split(|c: char| c.is_whitespace() || c == ',')
            .next()
            .unwrap_or("")
            .trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }
    // (syntax-02) Filename / stem lookup BEFORE the extension fallback
    // (claude-code `FILENAME_LANGS`): `Dockerfile`/`Makefile`/`CMakeLists.txt`/
    // `Rakefile`/`Gemfile` carry no extension but map to a language.
    if let Some(p) = path {
        let base = std::path::Path::new(p)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        let stem = base.split('.').next().unwrap_or("");
        if let Some(lang) = filename_language(base).or_else(|| filename_language(stem)) {
            return Some(lang.to_string());
        }
        // Fall back to the file extension.
        if let Some(ext) = std::path::Path::new(p).extension().and_then(|e| e.to_str()) {
            if !ext.is_empty() {
                return Some(ext.to_string());
            }
        }
    }
    if let Some(line) = first_line {
        if let Some(lang) = shebang_language(line) {
            return Some(lang.to_string());
        }
    }
    None
}

/// (syntax-01) claude-code `detectLanguage`'s shebang/first-line branch:
/// strip a UTF-8 BOM, then match `#!` interpreter lines and the PHP/XML
/// processing-instruction openers.
fn shebang_language(first_line: &str) -> Option<&'static str> {
    let line = first_line.strip_prefix('\u{feff}').unwrap_or(first_line);
    if line.starts_with("#!") {
        if line.contains("bash") || line.contains("/sh") {
            return Some("bash");
        }
        if line.contains("python") {
            return Some("python");
        }
        if line.contains("node") {
            return Some("javascript");
        }
        if line.contains("ruby") {
            return Some("ruby");
        }
        if line.contains("perl") {
            return Some("perl");
        }
        return None;
    }
    if line.starts_with("<?php") {
        return Some("php");
    }
    if line.starts_with("<?xml") {
        return Some("xml");
    }
    None
}

/// Language for a bare filename / stem (claude-code `FILENAME_LANGS`).
fn filename_language(name: &str) -> Option<&'static str> {
    match name {
        "Dockerfile" => Some("dockerfile"),
        "Makefile" => Some("makefile"),
        "Rakefile" | "Gemfile" => Some("ruby"),
        "CMakeLists" => Some("cmake"),
        _ => None,
    }
}

/// One plain (uncolored) StyledLine per input line — the fallback when the
/// language is unknown or absent.
fn plain_lines(code: &str) -> Vec<StyledLine> {
    code.lines()
        .map(|line| StyledLine {
            spans: vec![StyledSpan::plain(line)],
        })
        .collect()
}

/// Highlight `code` for `lang` (a fence info-string token or detected
/// language), themed by the active [`ThemeName`]'s syntect `.tmTheme`.
/// Unknown/None lang → one plain StyledLine per input line. Never panics.
///
/// (M7-15) Takes the active `ThemeName` (was `&TuiTheme`) so the bundled
/// `.tmTheme` follows the picker: switching the theme switches the syntect
/// palette for code blocks + diff previews.
#[must_use]
pub fn highlight(code: &str, lang: Option<&str>, theme: ThemeName) -> Vec<StyledLine> {
    if code.is_empty() {
        return Vec::new();
    }
    let ss = syntax_set();
    // Resolve the syntax: by token first (info-string), then by extension.
    let syntax = lang.and_then(|l| {
        ss.find_syntax_by_token(l)
            .or_else(|| ss.find_syntax_by_extension(l))
    });
    let Some(syntax) = syntax else {
        return plain_lines(code);
    };
    let tm = tm_theme_for(theme);
    let mut hl = HighlightLines::new(syntax, tm);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        // highlight_line never panics on the bundled syntaxes; on the rare
        // regex error, degrade that line to plain rather than unwrap-panic.
        let ranges = hl.highlight_line(line, ss).unwrap_or_default();
        let spans = if ranges.is_empty() {
            vec![StyledSpan::plain(line.trim_end_matches('\n'))]
        } else {
            // syntect's `LinesWithEndings` keeps the trailing "\n" as its own
            // final range; once `syn_span_to_styled` strips it that span is
            // zero-length but still carries the line's fg color. Drop those
            // empty-text spans so no highlighted line ends with a stray colored
            // empty span (which would inflate span counts / clutter snapshots).
            // A genuinely blank source line trims away to zero spans, leaving a
            // valid empty `StyledLine` so line counts / gutter numbering stay
            // correct.
            ranges
                .into_iter()
                .map(|(st, text)| syn_span_to_styled(st, text))
                .filter(|s| !s.text.is_empty())
                .collect()
        };
        out.push(StyledLine { spans });
    }
    out
}

/// Convert a syntect (Style, &str) range into a StyledSpan. Strips the
/// trailing newline so StyledLine spans never carry "\n". Code blocks keep
/// the terminal background; only the foreground is themed (bg = Default).
fn syn_span_to_styled(st: SynStyle, text: &str) -> StyledSpan {
    StyledSpan::styled(
        text.trim_end_matches('\n'),
        SpanStyle {
            fg: syn_color_to_style_color(st.foreground),
            bg: StyleColor::Default,
            bold: st.font_style.contains(FontStyle::BOLD),
            italic: st.font_style.contains(FontStyle::ITALIC),
            underline: st.font_style.contains(FontStyle::UNDERLINE),
        },
    )
}

/// Map a syntect 24-bit `Color` to the M7-01 `StyleColor::Rgb` (alpha dropped).
fn syn_color_to_style_color(c: SynColor) -> StyleColor {
    StyleColor::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A span is "colored" if its fg differs from the plain (terminal-default)
    /// foreground. Parity = equivalent look, not exact colors (§0 Q3).
    fn is_colored(s: &StyledSpan) -> bool {
        s.style.fg != StyleColor::Default
    }

    /// The class covering the first byte of `needle` in `line`.
    fn class_of(line: &str, lang: &str, needle: &str) -> SyntaxClass {
        let classes = classify_line(line, Some(lang));
        let at = line.find(needle).expect("needle present in line");
        class_at(&classes, at)
    }

    #[test]
    fn classify_line_labels_rust_tokens() {
        let line = "    let total = compute(1, 2); // sum";
        assert_eq!(class_of(line, "rs", "let"), SyntaxClass::Keyword);
        assert_eq!(class_of(line, "rs", "compute"), SyntaxClass::Function);
        assert_eq!(class_of(line, "rs", "1"), SyntaxClass::Number);
        assert_eq!(class_of(line, "rs", "// sum"), SyntaxClass::Comment);
    }

    #[test]
    fn classify_line_labels_strings_and_types() {
        let line = "struct Widget { name: String }";
        assert_eq!(class_of(line, "rs", "struct"), SyntaxClass::Keyword);
        assert_eq!(class_of(line, "rs", "Widget"), SyntaxClass::TypeName);
        let s = r#"let s = "hello";"#;
        assert_eq!(class_of(s, "rs", "hello"), SyntaxClass::StringLit);
    }

    #[test]
    fn classify_line_covers_the_whole_line_in_order() {
        let line = "fn main() {}";
        let classes = classify_line(line, Some("rs"));
        assert!(!classes.is_empty());
        // Ranges are ordered, non-overlapping, and span the full line.
        let mut cursor = 0usize;
        for (range, _) in &classes {
            assert_eq!(range.start, cursor, "gap or overlap at {cursor}");
            assert!(range.end > range.start);
            cursor = range.end;
        }
        assert_eq!(cursor, line.len(), "classes must cover the whole line");
    }

    #[test]
    fn classify_line_degrades_to_empty_for_unknown_language() {
        assert!(classify_line("whatever", None).is_empty());
        assert!(classify_line("whatever", Some("not-a-language")).is_empty());
        // An empty class list means "everything is Plain".
        assert_eq!(class_at(&[], 0), SyntaxClass::Plain);
    }

    #[test]
    fn classify_line_handles_multibyte_text_without_panicking() {
        // Byte offsets must stay on UTF-8 boundaries: this repo's own sources
        // are full of CJK comments, so a byte/char confusion here is not
        // hypothetical.
        let line = "let 名前 = \"值\"; // 中文注释 🙂";
        let classes = classify_line(line, Some("rs"));
        let mut cursor = 0usize;
        for (range, _) in &classes {
            assert!(line.is_char_boundary(range.start));
            assert!(line.is_char_boundary(range.end));
            assert_eq!(range.start, cursor);
            cursor = range.end;
        }
        assert_eq!(cursor, line.len());
    }

    #[test]
    fn detects_from_fence_info_string() {
        assert_eq!(detect_language(Some("rust"), None).as_deref(), Some("rust"));
        // info-string may carry extra metadata: "```rust,ignore" -> first token
        assert_eq!(
            detect_language(Some("rust,ignore"), None).as_deref(),
            Some("rust")
        );
        assert_eq!(
            detect_language(Some("python"), None).as_deref(),
            Some("python")
        );
    }

    #[test]
    fn detects_from_path_extension() {
        assert_eq!(
            detect_language(None, Some("src/main.rs")).as_deref(),
            Some("rs")
        );
        assert_eq!(
            detect_language(None, Some("a/b/app.py")).as_deref(),
            Some("py")
        );
        assert_eq!(
            detect_language(None, Some("data.json")).as_deref(),
            Some("json")
        );
    }

    #[test]
    fn fence_info_string_wins_over_path() {
        assert_eq!(
            detect_language(Some("js"), Some("file.py")).as_deref(),
            Some("js")
        );
    }

    #[test]
    fn no_lang_returns_none() {
        assert_eq!(detect_language(None, None), None);
        assert_eq!(detect_language(Some(""), None), None);
        // (syntax-02) A truly extension-less, unknown filename is still None.
        assert_eq!(detect_language(None, Some("README")), None);
    }

    #[test]
    fn filename_based_language_detection() {
        // (syntax-02) FILENAME_LANGS — basename and stem.
        let d = |p: &str| detect_language(None, Some(p));
        assert_eq!(d("Dockerfile").as_deref(), Some("dockerfile"));
        assert_eq!(d("Makefile").as_deref(), Some("makefile"));
        assert_eq!(d("Rakefile").as_deref(), Some("ruby"));
        assert_eq!(d("Gemfile").as_deref(), Some("ruby"));
        // Stem lookup: `CMakeLists.txt` → stem `CMakeLists` → cmake.
        assert_eq!(d("path/to/CMakeLists.txt").as_deref(), Some("cmake"));
        assert_eq!(d("Dockerfile.dev").as_deref(), Some("dockerfile"));
        // A normal extension still wins via the fallback.
        assert_eq!(d("main.rs").as_deref(), Some("rs"));
    }

    #[test]
    fn shebang_first_line_detection() {
        // (syntax-01) Only consulted when info-string AND filename/extension
        // both fail to resolve a language.
        let d = |line: &str| detect_language_with_first_line(None, None, Some(line));
        assert_eq!(d("#!/bin/bash").as_deref(), Some("bash"));
        assert_eq!(d("#!/bin/sh").as_deref(), Some("bash"));
        assert_eq!(d("#!/usr/bin/env python3").as_deref(), Some("python"));
        assert_eq!(d("#!/usr/bin/env node").as_deref(), Some("javascript"));
        assert_eq!(d("#!/usr/bin/ruby").as_deref(), Some("ruby"));
        assert_eq!(d("#!/usr/bin/perl").as_deref(), Some("perl"));
        assert_eq!(d("<?php").as_deref(), Some("php"));
        assert_eq!(d("<?xml version=\"1.0\"?>").as_deref(), Some("xml"));
        // UTF-8 BOM is stripped before matching.
        assert_eq!(d("\u{feff}#!/bin/bash").as_deref(), Some("bash"));
        // Unrecognized shebang interpreter -> None.
        assert_eq!(d("#!/usr/bin/env lua"), None);
        assert_eq!(d("plain text"), None);
    }

    #[test]
    fn shebang_only_consulted_as_last_resort() {
        // (syntax-01) A resolved extension wins over the shebang fallback.
        assert_eq!(
            detect_language_with_first_line(None, Some("main.rs"), Some("#!/bin/bash")).as_deref(),
            Some("rs")
        );
        // A resolved fence info-string wins too.
        assert_eq!(
            detect_language_with_first_line(Some("python"), None, Some("#!/bin/bash")).as_deref(),
            Some("python")
        );
    }

    #[test]
    fn highlight_rust_keeps_line_count_and_colors_some_spans() {
        let code = "fn main() {\n    let x = 1;\n}\n";
        let lines = highlight(code, Some("rust"), ThemeName::Dark);
        // STRUCTURE assertion (parity = equivalent look, not exact colors):
        assert_eq!(lines.len(), 3, "one StyledLine per source line");
        // At least one span on the keyword line is non-default colored.
        let any_colored = lines.iter().flat_map(|l| &l.spans).any(is_colored);
        assert!(
            any_colored,
            "rust highlight produced at least one colored span"
        );
    }

    #[test]
    fn highlight_unknown_lang_is_plain_one_span_per_line() {
        let lines = highlight("alpha\nbeta\n", Some("not-a-language"), ThemeName::Dark);
        assert_eq!(lines.len(), 2);
        for l in &lines {
            assert_eq!(l.spans.len(), 1, "plain fallback = single span per line");
            assert!(!is_colored(&l.spans[0]), "plain fallback span is uncolored");
        }
    }

    #[test]
    fn highlight_none_lang_is_plain() {
        let lines = highlight("just text\n", None, ThemeName::Dark);
        assert_eq!(lines.len(), 1);
        assert!(!is_colored(&lines[0].spans[0]));
    }

    #[test]
    fn highlight_empty_is_empty() {
        assert!(highlight("", Some("rust"), ThemeName::Dark).is_empty());
    }

    #[test]
    fn highlight_drops_trailing_empty_spans() {
        // syntect yields a final range for the trailing "\n"; after trimming it
        // becomes a zero-length span carrying the line's fg. None of those may
        // survive: no highlighted line may contain an empty-text span.
        let code = "fn main() {\n    let x = 1;\n}\n";
        let lines = highlight(code, Some("rust"), ThemeName::Dark);
        for l in &lines {
            assert!(
                l.spans.iter().all(|s| !s.text.is_empty()),
                "no highlighted span should have empty text: {:?}",
                l.spans
            );
        }
    }

    #[test]
    fn highlight_blank_line_still_yields_a_line() {
        // A blank line between two code lines must still produce its own
        // (possibly empty-span) StyledLine so line counts / gutter numbering
        // stay correct — it must not be dropped.
        let code = "let a = 1;\n\nlet b = 2;\n";
        let lines = highlight(code, Some("rust"), ThemeName::Dark);
        assert_eq!(lines.len(), 3, "blank middle line still counts as a line");
        // The blank middle line carries no non-empty content spans.
        assert!(
            lines[1].spans.iter().all(|s| s.text.is_empty()),
            "blank line has no non-empty spans: {:?}",
            lines[1].spans
        );
    }

    #[test]
    fn tm_theme_name_maps_to_bundled_names() {
        // The default syntect ThemeSet must contain whatever names we map to,
        // or HighlightLines::new would panic on the lookup in `highlight`.
        for n in ThemeName::ALL {
            let name = tm_theme_name(n);
            assert!(
                theme_set().themes.contains_key(name),
                "{name} is a bundled theme"
            );
        }
    }

    #[test]
    fn tm_theme_for_dark_and_light_differ() {
        // (M7-15) dark and light themes select DIFFERENT bundled tmThemes, so
        // the same code recolors when the picker switches.
        let d = tm_theme_for(ThemeName::Dark);
        let l = tm_theme_for(ThemeName::Light);
        assert_ne!(d.name, l.name);
    }
}
