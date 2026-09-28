//! CommonMark → [`StyledLine`] rendering via `pulldown-cmark`.
//!
//! Pure function: `render(text, theme) -> Vec<StyledLine>`. No iocraft, no
//! terminal, no async — colors are [`StyleColor`] values mapped to iocraft
//! only at draw time (`StyleColor::to_iocraft`).
//!
//! Literal reference: `claude-code/src/utils/markdown.ts` `formatToken`.
//! Handled here: paragraphs, headings, bold (`Strong`), italic (`Emphasis`),
//! inline code (`Code` → `theme.inline_code`), links (blue hyperlink text,
//! bare email for `mailto:`), ordered/unordered/nested lists, blockquote,
//! fenced code (emits a
//! [`SpanKind::CodePlaceholder`] span — M7-02 highlights it). Best-effort on
//! partial / unclosed input; never panics.
//!
//! Per claude-code: strikethrough is intentionally NOT parsed (the model
//! uses `~` for "approximately"); HTML/definitions render to nothing.

// `CommonMark`, `pulldown-cmark`, `claude-code` etc. read better unquoted in
// the module prose; suppress the doc-markdown nudge crate-wide for this file.
#![allow(clippy::doc_markdown)]

use crate::presentation::render::markdown_table::{self, ColumnAlign};
use crate::presentation::render::{SpanStyle, StyleColor, StyledLine, StyledSpan};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

/// Default render width (display columns) used by [`render`] when the caller
/// has no terminal width to thread in. Tables are the only block whose layout
/// depends on width; everything else is width-independent, so existing
/// non-table callers/tests are unaffected by this default.
const DEFAULT_RENDER_WIDTH: usize = 80;
const MARKDOWN_CACHE_CAP: usize = 64;

struct MarkdownCache {
    entries: VecDeque<(u64, Vec<StyledLine>)>,
}

static MARKDOWN_CACHE: Mutex<MarkdownCache> = Mutex::new(MarkdownCache {
    entries: VecDeque::new(),
});

fn markdown_cache_key(text: &str, theme: &MarkdownTheme, width: usize) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    width.hash(&mut hasher);
    format!("{theme:?}").hash(&mut hasher);
    hasher.finish()
}

/// Dim vertical bar prefixing blockquote lines. Matches claude-code's
/// `BLOCKQUOTE_BAR` (`src/constants/figures.ts`) — U+258E (▎ LEFT ONE QUARTER
/// BLOCK), NOT the box-drawing `│` (U+2502).
const BLOCKQUOTE_BAR: &str = "\u{258e}";

/// `numberToLetter` (claude-code `markdown.ts`): 1→a, 26→z, 27→aa, … (bijective
/// base-26, lowercase).
fn number_to_letter(mut n: u64) -> String {
    let mut result = String::new();
    while n > 0 {
        n -= 1;
        result.insert(0, char::from(b'a' + (n % 26) as u8));
        n /= 26;
    }
    result
}

/// `numberToRoman` (claude-code `markdown.ts`): lowercase roman numerals.
fn number_to_roman(mut n: u64) -> String {
    const ROMAN: &[(u64, &str)] = &[
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut result = String::new();
    for &(value, numeral) in ROMAN {
        while n >= value {
            result.push_str(numeral);
            n -= value;
        }
    }
    result
}

/// `getListNumber` (claude-code `markdown.ts`): ordered-list marker by nesting
/// `depth` — number at depth 0/1, lowercase letters at depth 2, lowercase roman
/// at depth 3, number beyond.
fn get_list_number(depth: usize, n: u64) -> String {
    match depth {
        0 | 1 => n.to_string(),
        2 => number_to_letter(n),
        3 => number_to_roman(n),
        _ => n.to_string(),
    }
}

/// Map a `pulldown-cmark` column [`Alignment`] to the table renderer's
/// [`ColumnAlign`]. `None` (no explicit alignment) is markdown's left default,
/// matching claude-code (`token.align?.[col] ?? 'left'`).
fn map_alignment(a: Alignment) -> ColumnAlign {
    match a {
        Alignment::Center => ColumnAlign::Center,
        Alignment::Right => ColumnAlign::Right,
        Alignment::None | Alignment::Left => ColumnAlign::Left,
    }
}

/// Theme colors the markdown renderer needs. Kept minimal and decoupled
/// from iocraft so the renderer is a pure value function.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownTheme {
    /// Inline-code (`codespan`) foreground — claude-code uses the
    /// `permission` theme color here.
    pub inline_code: StyleColor,
    /// (M7-15) Active theme for fenced-code-block syntect highlighting — the
    /// `.tmTheme` follows the picker. Plain-text oracles ignore it (they drop
    /// color); only the styled code-block path consumes it.
    pub code_theme: crate::presentation::theme::ThemeName,
}

/// Mutable inline styling state threaded through the event walk.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)]
struct InlineState {
    bold: bool,
    italic: bool,
    underline: bool,
    code: bool,
}

impl InlineState {
    fn to_style(self, theme: MarkdownTheme) -> SpanStyle {
        SpanStyle {
            fg: if self.code {
                theme.inline_code
            } else {
                StyleColor::Default
            },
            bg: StyleColor::Default,
            bold: self.bold,
            italic: self.italic,
            underline: self.underline,
        }
    }
}

/// Buffered fenced/indented code-block content awaiting M7-02 highlighting.
#[derive(Debug, Default)]
struct CodeBlockState {
    lang: Option<String>,
    text: String,
}

/// Render CommonMark `text` to styled lines using `theme`, at the
/// [`DEFAULT_RENDER_WIDTH`]. Strikethrough is disabled to match claude-code;
/// tables are enabled and now rendered as a bordered grid (see
/// [`render_with_width`]); footnotes are enabled by `pulldown-cmark` defaults
/// but only paragraph/heading/list/quote/code/table are styled here (others
/// fall through as their inline text).
///
/// Width only affects table layout; delegating to [`render_with_width`] with
/// the default keeps every non-table caller (and their pinned tests) unchanged.
#[must_use]
pub fn render(text: &str, theme: &MarkdownTheme) -> Vec<StyledLine> {
    render_with_width(text, theme, DEFAULT_RENDER_WIDTH)
}

/// Render CommonMark `text` to styled lines using `theme`, laying out markdown
/// tables to fit `width` display columns (the bordered-grid / vertical-fallback
/// algorithm in [`markdown_table`]). Non-table blocks are width-independent.
#[must_use]
pub fn render_with_width(text: &str, theme: &MarkdownTheme, width: usize) -> Vec<StyledLine> {
    let key = markdown_cache_key(text, theme, width);
    {
        let cache = MARKDOWN_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, lines)) = cache.entries.iter().find(|(k, _)| *k == key) {
            return lines.clone();
        }
    }

    let options = Options::ENABLE_TABLES;
    let parser = Parser::new_ext(text, options);
    let mut builder = Builder::new(theme, width);
    for event in parser {
        builder.handle(event);
    }
    let lines = builder.finish();

    let mut cache = MARKDOWN_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if cache.entries.len() >= MARKDOWN_CACHE_CAP {
        cache.entries.pop_front();
    }
    cache.entries.push_back((key, lines.clone()));
    lines
}

/// Accumulates styled lines while walking markdown events. Inline content is
/// appended to `pending` (the current line being built); block boundaries
/// flush `pending` into `lines`.
struct Builder<'a> {
    theme: &'a MarkdownTheme,
    lines: Vec<StyledLine>,
    pending: Vec<StyledSpan>,
    inline: InlineState,
    link_url: Option<String>,
    /// (md-03) Index into `pending` where the active link's text starts —
    /// lets `End(Link)` inspect/replace/recolor exactly the spans the link
    /// pushed, regardless of any nested emphasis/code inside the link text.
    link_start: usize,
    /// When inside `Start(Image)`/`End(Image)`: the image's destination URL.
    /// claude-code renders an image as just its href (`markdown.ts:139-140`),
    /// so the inner alt `Text` is suppressed and the URL emitted on close.
    image_url: Option<String>,
    /// Stack of list contexts (outer to inner). `Some(n)` = ordered list at
    /// next item number `n`; `None` = unordered.
    list_stack: Vec<Option<u64>>,
    /// Nesting depth of `Emphasis` spans. Italic is active while `> 0`. A
    /// counter (not a bool) so `End(Emphasis)` only clears the emphasis it
    /// owns and never the italic owned by an enclosing blockquote.
    emphasis_depth: u32,
    /// Nesting depth of blockquotes. While `> 0`, lines are prefixed with the
    /// bar and rendered italic. Independent of `emphasis_depth` so the two
    /// concerns can't stomp each other's italic.
    blockquote_depth: u32,
    /// When inside a fenced/indented code block: accumulates raw text and
    /// the language hint. `Some` between `Start(CodeBlock)`/`End(CodeBlock)`.
    code_block: Option<CodeBlockState>,
    /// Table render width (display columns) threaded from
    /// [`render_with_width`].
    table_width: usize,
    /// Per-column alignment from the table's `Start(Table(alignments))`.
    table_aligns: Vec<ColumnAlign>,
    /// Header cells (one styled span list per column), captured from the
    /// `TableHead` row.
    table_header: Vec<Vec<StyledSpan>>,
    /// Data rows: one row per `TableRow`, each a list of styled cells.
    table_rows: Vec<Vec<Vec<StyledSpan>>>,
    /// Total number of body rows seen (incl. rows dropped past
    /// [`markdown_table::MAX_TABLE_ROWS`]); overflow = this − cap drives the
    /// `… N more rows not shown` notice.
    table_total_rows: usize,
    /// The cell currently being built (flushed from `pending` on
    /// `End(TableCell)`).
    table_cell: Vec<StyledSpan>,
    /// The row currently being built (one styled-cell list per `TableCell`).
    table_row: Vec<Vec<StyledSpan>>,
    /// Whether the active row is the header row (`TableHead`) vs a data row.
    in_table_head: bool,
    /// Whether we are inside a `Start(Table)`/`End(Table)` span at all (so the
    /// cell-content path knows to buffer into `table_cell` not `lines`).
    in_table: bool,
}

impl<'a> Builder<'a> {
    fn new(theme: &'a MarkdownTheme, table_width: usize) -> Self {
        Builder {
            theme,
            lines: Vec::new(),
            pending: Vec::new(),
            inline: InlineState::default(),
            link_url: None,
            link_start: 0,
            image_url: None,
            list_stack: Vec::new(),
            emphasis_depth: 0,
            blockquote_depth: 0,
            code_block: None,
            table_width,
            table_aligns: Vec::new(),
            table_header: Vec::new(),
            table_rows: Vec::new(),
            table_total_rows: 0,
            table_cell: Vec::new(),
            table_row: Vec::new(),
            in_table_head: false,
            in_table: false,
        }
    }

    /// Flush the in-progress line (if any) into `lines`.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut spans = std::mem::take(&mut self.pending);
        if self.blockquote_depth > 0 {
            let mut prefixed = vec![StyledSpan::styled(
                format!("{BLOCKQUOTE_BAR} "),
                SpanStyle {
                    fg: StyleColor::Named(crate::presentation::render::NamedColor::BrightBlack),
                    ..SpanStyle::default()
                },
            )];
            prefixed.append(&mut spans);
            spans = prefixed;
        }
        self.lines.push(StyledLine { spans });
    }

    /// Append a styled-text span to the current line using current inline
    /// state.
    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Effective italic = inline emphasis OR an enclosing blockquote (or an
        // H1 heading's `inline.italic`). Derived here so emphasis and
        // blockquote own independent state and neither clears the other's bit.
        let mut state = self.inline;
        state.italic = state.italic || self.emphasis_depth > 0 || self.blockquote_depth > 0;
        self.pending
            .push(StyledSpan::styled(text, state.to_style(*self.theme)));
    }

    #[allow(clippy::too_many_lines, clippy::match_same_arms)]
    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(Tag::Strong) => self.inline.bold = true,
            Event::End(TagEnd::Strong) => self.inline.bold = false,
            Event::Start(Tag::Emphasis) => self.emphasis_depth += 1,
            Event::End(TagEnd::Emphasis) => {
                self.emphasis_depth = self.emphasis_depth.saturating_sub(1);
            }
            Event::Code(text) => {
                // inline code: force code color regardless of surrounding em.
                let prev = self.inline.code;
                self.inline.code = true;
                self.push_text(&text);
                self.inline.code = prev;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                // (md-03) Remember the URL + where the link's text starts in
                // `pending`, so `End(Link)` can inspect/replace/recolor it.
                self.link_url = Some(dest_url.to_string());
                self.link_start = self.pending.len();
            }
            Event::End(TagEnd::Link) => {
                // (md-03) claude-code: mailto links discard their display
                // text entirely and show the bare email (no styling, no
                // hyperlink). Other links are recolored blue (the
                // `createHyperlink` text color claude-code applies whether or
                // not the terminal supports real OSC-8 — the escape-sequence
                // wrapping itself needs a runtime terminal-support check this
                // port doesn't have, but the blue styling is unconditional);
                // when the link's text is empty or equals the URL itself
                // (claude-code shows just the URL in that case), the pushed
                // text is replaced with the bare URL. No more " (url)" suffix.
                if let Some(url) = self.link_url.take() {
                    if let Some(email) = url.strip_prefix("mailto:") {
                        self.pending.truncate(self.link_start);
                        self.push_text(email);
                    } else {
                        let plain: String = self.pending[self.link_start..]
                            .iter()
                            .map(|s| s.text.as_str())
                            .collect();
                        if plain.is_empty() || plain == url {
                            self.pending.truncate(self.link_start);
                            self.push_text(&url);
                        }
                        for span in &mut self.pending[self.link_start..] {
                            span.style.fg =
                                StyleColor::Named(crate::presentation::render::NamedColor::Blue);
                        }
                    }
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                // Remember the URL; suppress the alt text until the image closes.
                self.image_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Image) => {
                // claude-code renders the image as just its href.
                if let Some(url) = self.image_url.take() {
                    self.push_text(&url);
                }
            }
            Event::Text(text) => {
                if self.image_url.is_some() {
                    // Inside an image: the alt text is suppressed; the href is
                    // emitted on `End(Image)` (`markdown.ts:139-140`).
                } else if let Some(cb) = self.code_block.as_mut() {
                    cb.text.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.in_table {
                    // Inside a table cell a line break is whitespace; the cell
                    // wrap collapses it. (claude-code wraps the joined content.)
                    self.push_text(" ");
                } else {
                    self.flush();
                }
            }
            Event::End(TagEnd::Paragraph) => {
                self.flush();
                self.lines.push(StyledLine::empty());
            }
            Event::Start(Tag::Heading { level, .. }) => {
                self.flush();
                match level {
                    HeadingLevel::H1 => {
                        self.inline.bold = true;
                        self.inline.italic = true;
                        self.inline.underline = true;
                    }
                    _ => self.inline.bold = true,
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush();
                self.inline = InlineState::default();
                self.lines.push(StyledLine::empty());
            }
            Event::Rule => {
                // Thematic break → a literal "---" line. claude-code
                // (`markdown.ts:137-138`) emits exactly "---", not a
                // full-width horizontal rule.
                self.flush();
                self.lines.push(StyledLine {
                    spans: vec![StyledSpan::styled("---", SpanStyle::default())],
                });
            }
            Event::Start(Tag::List(first)) => {
                self.list_stack.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                self.list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                self.flush();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        // (md-02) Marker glyph by nesting depth (number / letter
                        // / roman) per claude-code `getListNumber`.
                        let m = format!("{}. ", get_list_number(depth, *n));
                        *n += 1;
                        m
                    }
                    _ => "- ".to_string(),
                };
                self.pending
                    .push(StyledSpan::plain(format!("{indent}{marker}")));
            }
            Event::End(TagEnd::Item) => {
                self.flush();
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.blockquote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let trimmed = info.trim();
                        // info-string may be "rust ignore" — take first word.
                        // `next()` yields `None` for whitespace-only info,
                        // collapsing to no language hint.
                        trimmed.split_whitespace().next().map(ToString::to_string)
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code_block = Some(CodeBlockState {
                    lang,
                    text: String::new(),
                });
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(cb) = self.code_block.take() {
                    self.emit_code_block(&cb);
                }
            }
            // ---- tables (claude-code `MarkdownTable`) ----------------------
            Event::Start(Tag::Table(alignments)) => {
                self.flush();
                self.in_table = true;
                self.table_aligns = alignments.iter().copied().map(map_alignment).collect();
                self.table_header.clear();
                self.table_rows.clear();
                self.table_total_rows = 0;
                self.table_row.clear();
                self.table_cell.clear();
                self.in_table_head = false;
                // A cell's content must not inherit list/quote context; the
                // builder is always entered at block level for a table.
                self.pending.clear();
            }
            Event::Start(Tag::TableHead) => {
                self.in_table_head = true;
                self.table_row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                self.table_header = std::mem::take(&mut self.table_row);
                self.in_table_head = false;
            }
            Event::Start(Tag::TableRow) => {
                self.table_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                if !self.in_table_head {
                    // Count every body row so the overflow past the cap is known
                    // even though we stop collecting their cells.
                    self.table_total_rows += 1;
                    if self.table_rows.len() < markdown_table::MAX_TABLE_ROWS {
                        self.table_rows.push(std::mem::take(&mut self.table_row));
                    }
                }
            }
            Event::Start(Tag::TableCell) => {
                // Cell inline content accumulates in `pending`; ensure it is
                // empty at cell start.
                self.pending.clear();
                self.table_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                // Flush the buffered inline spans into the current cell.
                let mut cell = std::mem::take(&mut self.pending);
                self.table_cell.append(&mut cell);
                self.table_row.push(std::mem::take(&mut self.table_cell));
            }
            Event::End(TagEnd::Table) => {
                self.in_table = false;
                let overflow_rows = self
                    .table_total_rows
                    .saturating_sub(markdown_table::MAX_TABLE_ROWS);
                let lines = markdown_table::render_table(
                    &self.table_header,
                    &self.table_rows,
                    &self.table_aligns,
                    self.table_width,
                    overflow_rows,
                    self.theme,
                );
                self.lines.extend(lines);
                self.lines.push(StyledLine::empty());
                self.table_aligns.clear();
                self.table_header.clear();
                self.table_rows.clear();
                self.table_total_rows = 0;
            }
            _ => {}
        }
    }

    /// M7-02: route a finished fenced/indented code block through syntect.
    /// `cb.lang` is the fence info-string (e.g. `rust`); empty/unknown → plain
    /// fallback. Emits one [`StyledLine`] per code line. Empty body → nothing.
    fn emit_code_block(&mut self, cb: &CodeBlockState) {
        // (syntax-01) No file path for a fenced block, but its own first
        // line stands in for the shebang/first-line heuristic.
        let first_line = cb.text.lines().next();
        let lang = crate::presentation::render::syntax::detect_language_with_first_line(
            cb.lang.as_deref(),
            None,
            first_line,
        );
        let highlighted = crate::presentation::render::syntax::highlight(
            &cb.text,
            lang.as_deref(),
            self.theme.code_theme,
        );
        if highlighted.is_empty() {
            // Preserve the previous behavior of an empty block still yielding a
            // (now empty-text) line so spacing/structure is stable.
            self.lines.push(StyledLine {
                spans: vec![StyledSpan::plain(String::new())],
            });
        } else {
            self.lines.extend(highlighted);
        }
    }

    fn finish(mut self) -> Vec<StyledLine> {
        // Defensive: an unterminated fence (streaming) leaves `code_block`
        // set with no `End(CodeBlock)` event — highlight its partial body so
        // the code still renders rather than vanishing.
        if let Some(cb) = self.code_block.take() {
            self.emit_code_block(&cb);
        }
        self.flush();
        // Drop a trailing blank line for tidy output.
        if matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::render::{SpanStyle, StyleColor};

    fn theme() -> MarkdownTheme {
        MarkdownTheme {
            inline_code: StyleColor::Named(crate::presentation::render::NamedColor::Magenta),
            code_theme: crate::presentation::theme::ThemeName::Dark,
        }
    }

    #[test]
    fn plain_paragraph() {
        let lines = render("hello world", &theme());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].plain_text(), "hello world");
        assert_eq!(lines[0].spans[0].style, SpanStyle::default());
    }

    #[test]
    fn bold_text() {
        let lines = render("a **b** c", &theme());
        // "a " | "b"(bold) | " c"
        let bold_span = lines[0].spans.iter().find(|s| s.text == "b").unwrap();
        assert!(bold_span.style.bold);
    }

    #[test]
    fn italic_text() {
        let lines = render("a *b* c", &theme());
        let it = lines[0].spans.iter().find(|s| s.text == "b").unwrap();
        assert!(it.style.italic);
    }

    #[test]
    fn inline_code_uses_theme_color() {
        let lines = render("run `cargo test` now", &theme());
        let code = lines[0]
            .spans
            .iter()
            .find(|s| s.text == "cargo test")
            .unwrap();
        assert_eq!(
            code.style.fg,
            StyleColor::Named(crate::presentation::render::NamedColor::Magenta)
        );
    }

    #[test]
    fn link_with_distinct_text_shows_only_text_colored_blue() {
        // (md-03) Display text differs from the URL: show just the text, no
        // " (url)" suffix, colored blue (claude-code `createHyperlink`).
        let lines = render("see [docs](https://x.io)", &theme());
        let joined = lines[0].plain_text();
        assert!(joined.contains("docs"));
        assert!(!joined.contains("https://x.io"), "{joined:?}");
        let link_span = lines[0]
            .spans
            .iter()
            .find(|s| s.text == "docs")
            .expect("the link text span");
        assert_eq!(
            link_span.style.fg,
            StyleColor::Named(crate::presentation::render::NamedColor::Blue)
        );
    }

    #[test]
    fn autolink_with_no_distinct_text_shows_bare_url_colored_blue() {
        // (md-03) When the display text equals the URL (e.g. an autolink),
        // claude-code shows just the URL — still blue.
        let lines = render("see <https://x.io>", &theme());
        let joined = lines[0].plain_text();
        assert!(joined.contains("https://x.io"));
        let link_span = lines[0]
            .spans
            .iter()
            .find(|s| s.text == "https://x.io")
            .expect("the URL span");
        assert_eq!(
            link_span.style.fg,
            StyleColor::Named(crate::presentation::render::NamedColor::Blue)
        );
    }

    #[test]
    fn mailto_link_shows_bare_email_not_blue() {
        // (md-03) mailto: links discard their display text and show the
        // bare email, unstyled (no hyperlink coloring).
        let lines = render("[Contact](mailto:a@b.com)", &theme());
        let joined = lines[0].plain_text();
        assert!(joined.contains("a@b.com"), "{joined:?}");
        assert!(!joined.contains("Contact"), "{joined:?}");
        assert!(!joined.contains("mailto:"), "{joined:?}");
        let email_span = lines[0]
            .spans
            .iter()
            .find(|s| s.text == "a@b.com")
            .expect("the email span");
        assert_eq!(email_span.style.fg, StyleColor::Default);
    }

    #[test]
    fn h1_is_bold_italic_underline() {
        let lines = render("# Title", &theme());
        let span = &lines[0].spans[0];
        assert_eq!(span.text, "Title");
        assert!(span.style.bold);
        assert!(span.style.italic);
        assert!(span.style.underline);
    }

    #[test]
    fn h2_is_bold_only() {
        let lines = render("## Sub", &theme());
        let span = &lines[0].spans[0];
        assert!(span.style.bold);
        assert!(!span.style.italic);
        assert!(!span.style.underline);
    }

    #[test]
    fn unordered_list_marker() {
        let lines = render("- one\n- two", &theme());
        let texts: Vec<String> = lines.iter().map(StyledLine::plain_text).collect();
        assert!(texts.iter().any(|t| t == "- one"));
        assert!(texts.iter().any(|t| t == "- two"));
    }

    #[test]
    fn ordered_list_marker() {
        let lines = render("1. first\n2. second", &theme());
        let texts: Vec<String> = lines.iter().map(StyledLine::plain_text).collect();
        assert!(texts.iter().any(|t| t == "1. first"));
        assert!(texts.iter().any(|t| t == "2. second"));
    }

    #[test]
    fn ordered_list_number_helpers() {
        // (md-02) numberToLetter / numberToRoman parity.
        assert_eq!(number_to_letter(1), "a");
        assert_eq!(number_to_letter(26), "z");
        assert_eq!(number_to_letter(27), "aa");
        assert_eq!(number_to_roman(1), "i");
        assert_eq!(number_to_roman(4), "iv");
        assert_eq!(number_to_roman(9), "ix");
        // getListNumber by depth: 0/1 number, 2 letter, 3 roman.
        assert_eq!(get_list_number(0, 2), "2");
        assert_eq!(get_list_number(1, 2), "2");
        assert_eq!(get_list_number(2, 2), "b");
        assert_eq!(get_list_number(3, 2), "ii");
    }

    #[test]
    fn ordered_list_marker_switches_glyph_by_depth() {
        // (md-02) Depth 0/1 = numbers, depth 2 = letters (a.), depth 3 = roman.
        let md = "1. top\n    1. mid\n        1. deep\n            1. deeper";
        let texts: Vec<String> = render(md, &theme())
            .iter()
            .map(StyledLine::plain_text)
            .collect();
        let joined = texts.join("\n");
        assert!(joined.contains("1. top"), "depth0 number: {joined:?}");
        assert!(joined.contains("1. mid"), "depth1 number: {joined:?}");
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("a. deep")),
            "depth2 letter: {joined:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.trim_start().starts_with("i. deeper")),
            "depth3 roman: {joined:?}"
        );
    }

    #[test]
    fn nested_list_indents() {
        let lines = render("- a\n  - b", &theme());
        let texts: Vec<String> = lines.iter().map(StyledLine::plain_text).collect();
        assert!(texts.iter().any(|t| t == "- a"));
        assert!(texts.iter().any(|t| t == "  - b"));
    }

    #[test]
    fn horizontal_rule_renders_three_dashes() {
        // claude-code emits exactly "---" for a thematic break (`***` here is
        // unambiguously a thematic break, never a setext underline).
        let lines = render("above\n\n***\n\nbelow", &theme());
        let texts: Vec<String> = lines.iter().map(StyledLine::plain_text).collect();
        assert!(
            texts.iter().any(|t| t == "---"),
            "expected a '---' line, got {texts:?}"
        );
        assert!(texts.iter().any(|t| t == "above"));
        assert!(texts.iter().any(|t| t == "below"));
    }

    #[test]
    fn image_renders_href_and_suppresses_alt() {
        // claude-code renders an image as just its href; the alt text is dropped.
        let lines = render("![the alt text](https://img.example/x.png)", &theme());
        let joined = lines
            .iter()
            .map(StyledLine::plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("https://img.example/x.png"),
            "href missing: {joined:?}"
        );
        assert!(
            !joined.contains("the alt text"),
            "alt text should be suppressed: {joined:?}"
        );
    }

    #[test]
    fn blockquote_has_bar_prefix() {
        let lines = render("> quoted", &theme());
        let line = lines
            .iter()
            .find(|l| l.plain_text().contains("quoted"))
            .unwrap();
        assert!(line.plain_text().starts_with("\u{258e} "));
        let text_span = line
            .spans
            .iter()
            .find(|s| s.text.contains("quoted"))
            .unwrap();
        assert!(text_span.style.italic);
    }

    #[test]
    fn emphasis_inside_blockquote_keeps_blockquote_italic() {
        // `> before *em* after`: the blockquote makes the whole line italic.
        // The inner emphasis must not clear the blockquote's italic for the
        // text that follows it.
        let lines = render("> before *em* after", &theme());
        let line = lines
            .iter()
            .find(|l| l.plain_text().contains("after"))
            .unwrap();
        let em = line.spans.iter().find(|s| s.text == "em").unwrap();
        assert!(em.style.italic, "emphasis text must be italic");
        let after = line
            .spans
            .iter()
            .find(|s| s.text.contains("after"))
            .unwrap();
        assert!(
            after.style.italic,
            "text after emphasis must remain italic (blockquote intact)"
        );
        let before = line
            .spans
            .iter()
            .find(|s| s.text.contains("before"))
            .unwrap();
        assert!(before.style.italic, "text before emphasis must be italic");
    }

    // M7-02: fenced code is now routed through syntect at render time, so
    // these blocks emit syntax-highlighted StyledLines, NOT a placeholder
    // span. Parity (§0 Q3): assert STRUCTURE (the code text appears, known
    // langs colorize at least one span), not exact per-token colors.

    #[test]
    fn fenced_rust_block_is_syntax_highlighted() {
        let md = "Here:\n\n```rust\nfn main() {}\n```\n";
        let lines = render(md, &theme());
        // Find the code line "fn main() {}" among the rendered lines.
        let code_line = lines
            .iter()
            .find(|l| l.plain_text().contains("fn main"))
            .expect("code line rendered");
        assert!(
            code_line
                .spans
                .iter()
                .any(|s| s.style.fg != StyleColor::Default),
            "fenced rust is syntax-highlighted, not a plain placeholder"
        );
        // No CodePlaceholder span survives.
        assert!(
            !lines.iter().flat_map(|l| &l.spans).any(|s| matches!(
                s.kind,
                crate::presentation::render::SpanKind::CodePlaceholder { .. }
            )),
            "placeholder is replaced by highlighted spans"
        );
    }

    #[test]
    fn fenced_unknown_lang_block_is_plain() {
        let md = "```klingon\nQapla'\n```\n";
        let lines = render(md, &theme());
        let code_line = lines
            .iter()
            .find(|l| l.plain_text().contains("Qapla"))
            .expect("code line rendered");
        assert!(
            code_line
                .spans
                .iter()
                .all(|s| s.style.fg == StyleColor::Default),
            "unknown-lang fence falls back to plain"
        );
    }

    #[test]
    fn fenced_no_lang_block_is_plain() {
        let md = "```\nplain code\n```";
        let lines = render(md, &theme());
        let code_line = lines
            .iter()
            .find(|l| l.plain_text().contains("plain code"))
            .expect("code line rendered");
        assert!(code_line
            .spans
            .iter()
            .all(|s| s.style.fg == StyleColor::Default));
    }

    #[test]
    fn unclosed_code_fence_does_not_panic_and_renders_partial() {
        // No closing ``` — streaming mid-block.
        let md = "intro\n```rust\nfn main() {";
        let lines = render(md, &theme());
        // intro paragraph present.
        assert!(lines.iter().any(|l| l.plain_text().contains("intro")));
        // the partial code still renders (now highlighted, not a placeholder).
        assert!(
            lines.iter().any(|l| l.plain_text().contains("fn main")),
            "unclosed fence still renders its partial body"
        );
    }

    #[test]
    fn unclosed_bold_does_not_panic() {
        let lines = render("text **still bold", &theme());
        assert!(lines.iter().any(|l| l.plain_text().contains("still bold")));
    }

    #[test]
    fn dangling_list_item_does_not_panic() {
        let _ = render("- a\n- ", &theme());
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(render("", &theme()).is_empty());
    }

    #[test]
    fn lone_special_chars_do_not_panic() {
        let _ = render("*_`#>[](", &theme());
        let _ = render("```", &theme());
    }

    #[test]
    fn snapshot_heading() {
        insta::assert_yaml_snapshot!(render("# Title\n\n## Sub", &theme()));
    }

    #[test]
    fn snapshot_bold_italic() {
        insta::assert_yaml_snapshot!(render("normal **bold** and *italic* mix", &theme()));
    }

    #[test]
    fn snapshot_nested_list() {
        insta::assert_yaml_snapshot!(render("- a\n  - b\n  - c\n- d", &theme()));
    }

    #[test]
    fn snapshot_ordered_list() {
        insta::assert_yaml_snapshot!(render("1. first\n2. second\n3. third", &theme()));
    }

    #[test]
    fn snapshot_blockquote() {
        insta::assert_yaml_snapshot!(render("> quoted line\n> second", &theme()));
    }

    #[test]
    fn snapshot_inline_code() {
        insta::assert_yaml_snapshot!(render("run `cargo test --workspace` now", &theme()));
    }

    #[test]
    fn snapshot_link() {
        insta::assert_yaml_snapshot!(render("see [the docs](https://example.io/guide)", &theme()));
    }

    /// Render fenced-code lines structurally: per span "[C]"/"[P]" (colored /
    /// plain) + the text. M7-02 routes fences through syntect, so concrete
    /// colors are NOT snapshotted (parity §0 Q3) — only which spans colorize.
    fn fence_structure(lines: &[StyledLine]) -> String {
        let mut out = String::new();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for s in &l.spans {
                let flag = if s.style.fg == StyleColor::Default {
                    'P'
                } else {
                    'C'
                };
                out.push('[');
                out.push(flag);
                out.push(']');
                out.push_str(&s.text);
            }
        }
        out
    }

    #[test]
    fn snapshot_fenced_code_highlighted() {
        // M7-02: ```rust fence is syntect-highlighted (structure, not color).
        insta::assert_snapshot!(fence_structure(&render(
            "```rust\nfn main() {}\n```",
            &theme()
        )));
    }

    #[test]
    fn snapshot_partial_unclosed_fence() {
        insta::assert_snapshot!(fence_structure(&render(
            "text\n```python\nprint(1)",
            &theme()
        )));
    }

    // ---- A2: markdown TABLE grid layout --------------------------------

    const TABLE_MD: &str = "\
| Name | Role |
|:-----|-----:|
| Ada  | Eng  |
| Bob  | PM   |";

    #[test]
    fn table_renders_grid_not_inline() {
        let lines = render(TABLE_MD, &theme());
        let joined: String = lines
            .iter()
            .map(StyledLine::plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        // Bordered grid glyphs present (not a flattened inline string).
        assert!(joined.contains('│'), "vertical border present: {joined:?}");
        assert!(joined.contains('┌'), "top-left corner present");
        assert!(joined.contains('┼'), "interior cross present");
        // Header text rendered.
        assert!(joined.contains("Name") && joined.contains("Role"));
        // Data rows rendered.
        assert!(joined.contains("Ada") && joined.contains("Bob"));
    }

    #[test]
    fn table_maps_column_alignment() {
        // Left + right aligned columns from `:---` / `---:`.
        let lines = render(TABLE_MD, &theme());
        // The header row is centered regardless; the data rows honor the
        // per-column alignment. Find a data line containing "Ada".
        let ada_line = lines
            .iter()
            .map(StyledLine::plain_text)
            .find(|l| l.contains("Ada"))
            .expect("data row with Ada");
        // Left-aligned col 1: content hugs the left after "│ ".
        assert!(ada_line.starts_with("│ Ada"), "left col: {ada_line:?}");
        // Right-aligned col 2: "Eng" hugs the right before " │".
        assert!(ada_line.ends_with("Eng │"), "right col: {ada_line:?}");
    }

    #[test]
    fn table_inline_code_cell_keeps_style() {
        // A cell containing inline code keeps the inline-code color span.
        let md = "| A |\n|---|\n| `x` |";
        let lines = render(md, &theme());
        let code = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.text == "x")
            .expect("inline-code cell span present");
        assert_eq!(
            code.style.fg,
            StyleColor::Named(crate::presentation::render::NamedColor::Magenta)
        );
    }

    #[test]
    fn snapshot_markdown_table() {
        // Default width (80) → bordered grid.
        insta::assert_yaml_snapshot!(render(TABLE_MD, &theme()));
    }

    #[test]
    fn snapshot_markdown_table_narrow() {
        // Narrow width (24) → exercises the shrink / vertical-fallback paths.
        insta::assert_yaml_snapshot!(render_with_width(TABLE_MD, &theme(), 24));
    }

    #[test]
    fn table_over_200_rows_appends_not_shown_notice() {
        // A table with more than MAX_TABLE_ROWS body rows caps at 200 and
        // appends the `… N more rows not shown` notice (parity 2.1.208).
        let total = markdown_table::MAX_TABLE_ROWS + 42;
        let mut md = String::from("| id |\n|---|\n");
        for i in 0..total {
            md.push_str(&format!("| {i} |\n"));
        }
        let lines = render(&md, &theme());
        let joined: String = lines
            .iter()
            .map(StyledLine::plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        // Rows beyond the cap are dropped but their count is surfaced.
        assert!(joined.contains("199"));
        assert!(!joined.contains("| 200"));
        assert!(joined.contains("… 42 more rows not shown"));
    }

    #[test]
    fn table_at_or_below_200_rows_has_no_notice() {
        let mut md = String::from("| id |\n|---|\n");
        for i in 0..markdown_table::MAX_TABLE_ROWS {
            md.push_str(&format!("| {i} |\n"));
        }
        let lines = render(&md, &theme());
        let joined: String = lines
            .iter()
            .map(StyledLine::plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains("not shown"));
    }
}
