//! StructuredDiff viewer (M7-02) — similar line+word diff, syntect-colored.
//!
//! Layout parity: claude-code/src/components/StructuredDiff/Fallback.tsx.
//!
//! Locked structural facts (from `Fallback.tsx` — `transformLinesToObjects`,
//! `processAdjacentLines`, `formatDiff`):
//!   - Each diff line is `add` / `remove` / `nochange` (context).
//!   - Gutter = right-aligned line number (`padStart(maxWidth)`) + one space;
//!     then the sigil column (`+` add / `-` remove / ` ` context) + one space;
//!     then the syntax-colored content.
//!   - `maxWidth` = width of the largest line number.
//!   - Added lines get a green line background (`diffAdded`), removed lines red
//!     (`diffRemoved`); content keeps its syntax fg layered over the line bg.
//!   - Adjacent remove→add runs are paired for word-level diffing;
//!     `CHANGE_THRESHOLD = 0.4` decides word-diff vs. plain full-line coloring
//!     when the two lines are too dissimilar.
//!
//! M7-01 type note: `StyledSpan` is `{ text, style: SpanStyle { fg, bg, bold,
//! .. }, kind }`. Diff backgrounds are `StyleColor::Rgb` values set on
//! `style.bg`; the dim gutter uses `StyleColor::Named(BrightBlack)`.

// `claude-code`, `Fallback.tsx`, `diffAdded` etc. read better unquoted in the
// module prose; suppress the doc-markdown nudge crate-wide for this file
// (matches `render/markdown.rs`).
#![allow(clippy::doc_markdown)]

use similar::{ChangeTag, TextDiff};

use crate::render::syntax;
use crate::render::{NamedColor, SpanStyle, StyleColor, StyledLine, StyledSpan};
use crate::theme::ThemeName;

/// Classification of one rendered diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Inserted line (new file only).
    Add,
    /// Deleted line (old file only).
    Remove,
    /// Unchanged context line.
    Context,
}

/// One row in the structured diff before layout.
#[derive(Debug, Clone)]
pub struct DiffRow {
    /// Add / Remove / Context classification.
    pub kind: LineKind,
    /// Line text with the trailing newline stripped.
    pub text: String,
    /// New-file line number for Add/Context; old-file line number for Remove.
    pub line_no: usize,
}

/// (EXPERIMENT) Diff backgrounds are rendered at this alpha over the dark
/// terminal background. Terminals have NO real background alpha, so this blends
/// the opaque diff color toward black: `result = color * DIFF_BG_ALPHA`.
/// Numerator/denominator kept as integers for a `const fn` blend.
const DIFF_BG_ALPHA_NUM: u16 = 1;
const DIFF_BG_ALPHA_DEN: u16 = 2; // 0.5

/// Blend an opaque `rgb` diff background toward the dark terminal background
/// (black) at [`DIFF_BG_ALPHA_NUM`]/[`DIFF_BG_ALPHA_DEN`] — a stand-in for a
/// translucent background the terminal cannot render natively.
const fn alpha_over_black(r: u8, g: u8, b: u8) -> StyleColor {
    StyleColor::Rgb(
        (r as u16 * DIFF_BG_ALPHA_NUM / DIFF_BG_ALPHA_DEN) as u8,
        (g as u16 * DIFF_BG_ALPHA_NUM / DIFF_BG_ALPHA_DEN) as u8,
        (b as u16 * DIFF_BG_ALPHA_NUM / DIFF_BG_ALPHA_DEN) as u8,
    )
}

/// Green background for added lines — claude-code's dark-theme `diffAdded`
/// (`rgb(34,92,43)`) rendered at [`DIFF_BG_ALPHA_NUM`]/[`DIFF_BG_ALPHA_DEN`]
/// alpha over the terminal background (EXPERIMENT — softer, translucent look).
#[must_use]
pub fn add_bg(_theme: ThemeName) -> StyleColor {
    alpha_over_black(34, 92, 43)
}

/// Red background for removed lines — claude-code `diffRemoved` (`rgb(122,41,54)`)
/// at the same alpha over the terminal background.
#[must_use]
pub fn remove_bg(_theme: ThemeName) -> StyleColor {
    alpha_over_black(122, 41, 54)
}

/// (diff-02) Green decoration color for the Add gutter (sigil + line number)
/// — claude-code `theme.addDecoration` (`rgb(80, 200, 80)`, dark-theme
/// value), locked independent of the active theme like `add_bg`/`remove_bg`.
#[must_use]
pub fn add_decoration(_theme: ThemeName) -> StyleColor {
    StyleColor::Rgb(80, 200, 80)
}

/// (diff-02) Red decoration color for the Remove gutter — claude-code
/// `theme.deleteDecoration` (`rgb(220, 90, 90)`).
#[must_use]
pub fn remove_decoration(_theme: ThemeName) -> StyleColor {
    StyleColor::Rgb(220, 90, 90)
}

/// Foreground color for removed-line content text — claude-code's syntax-diff
/// `foreground` (`rgb(248,248,242)`, dark-theme), the near-white default fg the
/// diff renderer layers over the removed-line background. Locked to the
/// dark-theme value like `remove_bg`.
#[must_use]
pub fn remove_text_fg(_theme: ThemeName) -> StyleColor {
    StyleColor::Rgb(248, 248, 242)
}

/// claude-code CHANGE_THRESHOLD: above this changed-fraction, word diffing is
/// abandoned for whole-line coloring (lines too dissimilar to align words).
const CHANGE_THRESHOLD: f64 = 0.4;

/// Brighter emphasis background for the changed *words* of a paired add line —
/// claude-code `diffAddedWord` (`rgb(56,166,96)`) at the diff-bg alpha.
#[must_use]
pub fn add_word_bg(_theme: ThemeName) -> StyleColor {
    alpha_over_black(56, 166, 96)
}

/// Brighter emphasis background for the changed *words* of a paired remove
/// line — claude-code `diffRemovedWord` (`rgb(179,89,107)`) at the diff-bg alpha.
#[must_use]
pub fn remove_word_bg(_theme: ThemeName) -> StyleColor {
    alpha_over_black(179, 89, 107)
}

/// Classify a single `similar` line change into a [`DiffRow`] — the ONE source
/// of truth for diff line classification. Maps the change tag to a [`LineKind`],
/// strips the trailing newline, and derives the line number from the change's
/// file index (claude-code numbering: removed lines against the old file,
/// added/context against the new file). The `grouped_hunks` production path is
/// the sole caller; keeping classification here means there is exactly one
/// place that decides add/remove/context + line number.
fn classify_change(change: &similar::Change<&str>) -> DiffRow {
    let text = change.value().trim_end_matches('\n').to_string();
    let (kind, line_no) = match change.tag() {
        ChangeTag::Delete => (LineKind::Remove, change.old_index().map_or(0, |i| i + 1)),
        ChangeTag::Insert => (LineKind::Add, change.new_index().map_or(0, |i| i + 1)),
        ChangeTag::Equal => (LineKind::Context, change.new_index().map_or(0, |i| i + 1)),
    };
    DiffRow {
        kind,
        text,
        line_no,
    }
}

/// Sigil character for a line kind: '+' / '-' / ' '.
fn sigil(kind: LineKind) -> char {
    match kind {
        LineKind::Add => '+',
        LineKind::Remove => '-',
        LineKind::Context => ' ',
    }
}

/// Build the gutter span ("  12 + ") for a row: right-aligned line number +
/// space + sigil + space, carrying the line background. (diff-02) Colored
/// green/red for Add/Remove (claude-code `decorationColor`); dim BrightBlack
/// only for Context.
fn gutter_span(
    kind: LineKind,
    line_no: usize,
    gutter_w: usize,
    bg: StyleColor,
    theme: ThemeName,
) -> StyledSpan {
    let text = format!("{line_no:>gutter_w$} {} ", sigil(kind));
    let fg = match kind {
        LineKind::Add => add_decoration(theme),
        LineKind::Remove => remove_decoration(theme),
        LineKind::Context => StyleColor::Named(NamedColor::BrightBlack),
    };
    StyledSpan::styled(
        text,
        SpanStyle {
            fg,
            bg,
            ..SpanStyle::default()
        },
    )
}

/// One contiguous run of a diff row's text, classified for BOTH the terminal
/// and portable clients.
///
/// Concatenating a row's `text` fields reproduces the row's text exactly. That
/// is the whole point: the wire ships pre-split segments rather than byte
/// ranges, because Rust indexes strings by UTF-8 byte, Swift by grapheme
/// cluster, and Kotlin/JS by UTF-16 code unit — one `(start, end)` pair means
/// four different substrings on four surfaces.
// The four bools are independent, orthogonal facts about one run: three are
// the syntect font style (mirroring `SpanStyle`'s own bold/italic/underline)
// and the fourth is the semantic word-diff emphasis. Packing them into a
// bitflags type would obscure the 1:1 correspondence with `SpanStyle` that
// makes `style_structured`'s reconstruction verifiably lossless.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeSegment {
    /// Text of this run.
    pub text: String,
    /// Portable semantic class; clients map it to their own light/dark palette.
    pub class: syntax::SyntaxClass,
    /// Resolved TERMINAL foreground (a syntect color, or the removed-line fg).
    /// Baked against one dark `.tmTheme` — clients should prefer `class`.
    pub fg: StyleColor,
    /// Bold, per the syntect theme.
    pub bold: bool,
    /// Italic, per the syntect theme.
    pub italic: bool,
    /// Underline, per the syntect theme.
    pub underline: bool,
    /// This run is a changed *word* of a word-diffed pair, so it takes the
    /// brighter emphasis background instead of the plain line background.
    pub emph: bool,
}

impl CodeSegment {
    /// A plain, unemphasized run in `fg`.
    fn plain(text: impl Into<String>, fg: StyleColor) -> Self {
        CodeSegment {
            text: text.into(),
            class: syntax::SyntaxClass::Plain,
            fg,
            bold: false,
            italic: false,
            underline: false,
            emph: false,
        }
    }
}

/// One fully derived diff row, before any terminal layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredDiffRow {
    /// Add / Remove / Context.
    pub kind: LineKind,
    /// New-file line number for Add/Context; old-file line number for Remove.
    pub line_no: usize,
    /// 0-based hunk index. A change between consecutive rows is where the dim
    /// `...` separator belongs (claude-code never renders `@@` headers).
    pub hunk: usize,
    /// This row was paired for word-level diffing, so its segments carry
    /// `emph` flags and no syntax highlighting.
    pub word_diffed: bool,
    /// Content runs, left to right. Excludes the gutter.
    pub segments: Vec<CodeSegment>,
}

impl StructuredDiffRow {
    /// The row's text, reassembled from its segments.
    #[must_use]
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect()
    }
}

/// A complete diff, derived once and rendered by any surface.
///
/// [`style_structured`] turns this into terminal lines; `client-adapter`
/// lowers it onto the wire for mobile and desktop. Both consume the SAME
/// hunk grouping, line classification, and `CHANGE_THRESHOLD` decision, which
/// is what keeps the four surfaces from drifting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StructuredDiff {
    /// Emitted rows, in order, across all hunks.
    pub rows: Vec<StructuredDiffRow>,
    /// Width of the right-aligned line-number gutter, computed across ALL
    /// hunks so the gutter does not jitter between them.
    pub gutter_width: usize,
    /// Whole-diff added-line count (not just emitted rows).
    pub additions: usize,
    /// Whole-diff removed-line count (not just emitted rows).
    pub removals: usize,
    /// Rows dropped by the row cap; `0` when the diff is complete.
    pub truncated_rows: usize,
    /// The row cap was reached exactly at a hunk boundary, after the terminal
    /// renderer had already committed to that hunk's separator. Recorded so
    /// [`style_structured`] stays byte-identical to the pre-refactor renderer;
    /// wire consumers ignore it.
    pub dangling_separator: bool,
    /// Detected syntect language token (e.g. `"Rust"`), if any.
    pub language: Option<String>,
}

/// Syntax-highlight `text` as a single line into classified segments.
///
/// Span boundaries come from [`syntax::highlight`] unchanged; the semantic
/// class is looked up per span from [`syntax::classify_line`]. Deriving the
/// class this way — attach, never re-split — is what guarantees the terminal
/// output is untouched by the addition of classes.
fn content_segments(text: &str, lang: Option<&str>, theme: ThemeName) -> Vec<CodeSegment> {
    let highlighted = syntax::highlight(text, lang, theme);
    let Some(first) = highlighted.into_iter().next() else {
        // Empty line — still emit one (empty) run so the row colors fully.
        return vec![CodeSegment::plain(String::new(), StyleColor::Default)];
    };
    let classes = syntax::classify_line(text, lang);
    let mut offset = 0usize;
    first
        .spans
        .into_iter()
        .map(|s| {
            let class = syntax::class_at(&classes, offset);
            offset += s.text.len();
            CodeSegment {
                text: s.text,
                class,
                fg: s.style.fg,
                bold: s.style.bold,
                italic: s.style.italic,
                underline: s.style.underline,
                emph: false,
            }
        })
        .collect()
}

/// Build the content segments for one line of a paired word-diff. `is_add`
/// selects which side's changes to emphasize. Returns `None` if the two lines
/// are too dissimilar (caller falls back to whole-line coloring).
fn word_diff_segments(
    remove_text: &str,
    add_text: &str,
    is_add: bool,
    theme: ThemeName,
) -> Option<Vec<CodeSegment>> {
    let wd = TextDiff::from_words(remove_text, add_text);
    // Changed fraction = changed words / total words on this side.
    //
    // (diff-04) claude-code's `wordDiffStrings` uses a symmetric CHAR-LENGTH
    // ratio (changedLen / (oldLen+newLen) > 0.4) rather than this per-side
    // word-count. Porting the ratio alone REGRESSES, because it is coupled to
    // the TOKENIZER: `similar`'s `from_words` produces coarser tokens than
    // TS's punctuation-splitting `tokenize` (e.g. `oldName(param)` is not
    // split on `(`), so the char-length ratio comes out above 0.4 and skips
    // word-diff on cases TS keeps. Matching TS requires porting its tokenizer
    // too — the larger work tracked separately; the word-count heuristic
    // stays until then.
    let (mut changed, mut total) = (0usize, 0usize);
    for ch in wd.iter_all_changes() {
        match ch.tag() {
            ChangeTag::Equal => total += 1,
            ChangeTag::Delete => {
                if !is_add {
                    total += 1;
                    changed += 1;
                }
            }
            ChangeTag::Insert => {
                if is_add {
                    total += 1;
                    changed += 1;
                }
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let too_dissimilar = total == 0 || (changed as f64 / total as f64) > CHANGE_THRESHOLD;
    if too_dissimilar {
        return None;
    }
    let fg = if is_add {
        StyleColor::Default
    } else {
        remove_text_fg(theme)
    };
    let mut segments = Vec::new();
    for ch in wd.iter_all_changes() {
        let show = matches!(ch.tag(), ChangeTag::Equal)
            || (is_add && ch.tag() == ChangeTag::Insert)
            || (!is_add && ch.tag() == ChangeTag::Delete);
        if !show {
            continue;
        }
        let emphasized = !matches!(ch.tag(), ChangeTag::Equal);
        segments.push(CodeSegment {
            emph: emphasized,
            ..CodeSegment::plain(ch.value(), fg)
        });
    }
    Some(segments)
}

/// Derive a single non-word-diffed row (context, or unpaired/too-dissimilar
/// add/remove): whole-line syntax-colored content.
fn plain_structured_row(
    row: &DiffRow,
    hunk: usize,
    lang: Option<&str>,
    theme: ThemeName,
) -> StructuredDiffRow {
    // (diff-01) Removed lines render as PLAINTEXT in claude-code (no syntax
    // highlight) — only the red bg decoration marks them. Added/context lines
    // keep syntax coloring.
    let segments = if row.kind == LineKind::Remove {
        vec![CodeSegment::plain(row.text.clone(), remove_text_fg(theme))]
    } else {
        content_segments(&row.text, lang, theme)
    };
    StructuredDiffRow {
        kind: row.kind,
        line_no: row.line_no,
        hunk,
        word_diffed: false,
        segments,
    }
}

/// Derive a word-diffed row from its already-computed emphasis segments.
fn word_structured_row(
    row: &DiffRow,
    hunk: usize,
    segments: Vec<CodeSegment>,
) -> StructuredDiffRow {
    StructuredDiffRow {
        kind: row.kind,
        line_no: row.line_no,
        hunk,
        word_diffed: true,
        segments,
    }
}

/// Hard cap on rendered diff body lines (claude-code shows a "… N more lines"
/// footer past a budget). Mirrors the M6 UserToolResult MAX_LINES intent.
pub const MAX_DIFF_LINES: usize = 100;

/// Row cap for a diff crossing the FFI / websocket to a client.
///
/// Higher than [`MAX_DIFF_LINES`] because a phone screen scrolls where a
/// terminal viewport does not, but still bounded: a 5 000-line `Write` must
/// not ship 5 000 rows × their segments to four clients. The untruncated text
/// is always still available in the tool result's own payload.
pub const MAX_WIRE_DIFF_ROWS: usize = 400;

// A client scrolls where a terminal viewport does not, so the wire cap must be
// the looser of the two. Asserted at COMPILE time: a runtime assertion over two
// constants is optimized away and can never fail.
const _: () = assert!(MAX_WIRE_DIFF_ROWS > MAX_DIFF_LINES);

/// Context radius for grouped (unified-diff) hunks — 3 lines, matching the
/// unified-diff default.
const CONTEXT_RADIUS: usize = 3;

/// Dim `...` separator drawn BETWEEN hunks (claude-code `StructuredDiffList`
/// "ellipsis separators between them"). Replaces unified `@@` headers, which
/// claude-code never renders.
fn hunk_separator() -> StyledLine {
    StyledLine {
        spans: vec![StyledSpan::styled(
            "...".to_string(),
            SpanStyle {
                fg: StyleColor::Named(NamedColor::BrightBlack),
                ..SpanStyle::default()
            },
        )],
    }
}

/// A "… {n} more lines" truncation footer (dim).
fn truncation_footer(n: usize) -> StyledLine {
    StyledLine {
        spans: vec![StyledSpan::styled(
            format!("… {n} more lines"),
            SpanStyle {
                fg: StyleColor::Named(NamedColor::BrightBlack),
                ..SpanStyle::default()
            },
        )],
    }
}

/// A unified-diff hunk header quadruple: `(old_start, old_len, new_start,
/// new_len)` with 1-based starts.
type HunkHeader = (usize, usize, usize, usize);

/// Group the diff into unified-diff hunks. Returns, per hunk, the header
/// quadruple and the rows of that hunk.
fn grouped_hunks(old: &str, new: &str) -> Vec<(HunkHeader, Vec<DiffRow>)> {
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(CONTEXT_RADIUS) {
        if group.is_empty() {
            continue;
        }
        let old_start = group.first().map_or(0, |op| op.old_range().start);
        let old_end = group.last().map_or(0, |op| op.old_range().end);
        let new_start = group.first().map_or(0, |op| op.new_range().start);
        let new_end = group.last().map_or(0, |op| op.new_range().end);
        let mut rows = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                rows.push(classify_change(&change));
            }
        }
        hunks.push((
            (
                old_start + 1,
                old_end - old_start,
                new_start + 1,
                new_end - new_start,
            ),
            rows,
        ));
    }
    hunks
}

/// (fileedit-result-added-removed-header) Count of added/removed lines
/// between `old` and `new` — claude-code's `structuredPatch.reduce(...)` line
/// counts, for the `Added N line(s)[, removed M line(s)]` summary header.
#[must_use]
pub fn diff_stats(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut additions = 0usize;
    let mut removals = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => removals += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, removals)
}

/// Render a structured diff of `old` → `new`. `path` drives syntax language
/// detection (claude-code's `filePath` prop). Hunks are separated by unified
/// `@@` headers; output past `MAX_DIFF_LINES` body rows is truncated with a
/// "… N more lines" footer. Never panics. Equivalent to
/// [`render_with_width`] with `width = 0` (no right-edge padding — see
/// diff-03 there).
#[must_use]
pub fn render(old: &str, new: &str, path: Option<&str>, theme: ThemeName) -> Vec<StyledLine> {
    render_with_width(old, new, path, theme, 0)
}

/// (diff-03) [`render`], plus padding changed (Add/Remove) rows with spaces
/// carrying the line background out to `width` total columns (claude-code
/// `wrapText`'s "pad changed lines so background extends to edge"). Context
/// rows are never padded (terminal-default background). `width = 0` (or any
/// value at/below the gutter width) disables padding — every row already
/// reaches or exceeds that, so the pad-to-width never has anything to add.
#[must_use]
pub fn render_with_width(
    old: &str,
    new: &str,
    path: Option<&str>,
    theme: ThemeName,
    width: usize,
) -> Vec<StyledLine> {
    style_structured(
        &structured_diff(old, new, path, theme, MAX_DIFF_LINES),
        theme,
        width,
    )
}

/// Derive the complete structured diff of `old` → `new`, capping emitted body
/// rows at `max_rows`.
///
/// Pure and terminal-free. `theme` selects the syntect palette for the baked
/// [`CodeSegment::fg`] values only — the portable [`CodeSegment::class`] is
/// theme-independent. This is the single derivation every surface consumes:
/// [`style_structured`] for the terminal, `client-adapter` for the wire.
#[must_use]
pub fn structured_diff(
    old: &str,
    new: &str,
    path: Option<&str>,
    theme: ThemeName,
    max_rows: usize,
) -> StructuredDiff {
    let hunks = grouped_hunks(old, new);
    if hunks.is_empty() {
        return StructuredDiff::default();
    }
    // Gutter width = widest line number across ALL hunks, right-aligned, so
    // the gutter does not jitter between them.
    let max_no = hunks
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|r| r.line_no))
        .max()
        .unwrap_or(1);
    let gutter_width = max_no.to_string().len();
    // (syntax-01) `new` wins for the first-line/shebang heuristic (it's the
    // post-edit content); a pure-delete diff (`new` empty) falls back to `old`.
    let first_line = new.lines().next().or_else(|| old.lines().next());
    let language = syntax::detect_language_with_first_line(None, path, first_line);
    let (additions, removals) = diff_stats(old, new);

    // Total body rows across all hunks — used for the truncation footer count.
    let total_body: usize = hunks.iter().map(|(_, rows)| rows.len()).sum();

    let mut rows: Vec<StructuredDiffRow> = Vec::new();
    let mut truncated = false;
    let mut dangling_separator = false;
    'hunks: for (hunk, (_header, hunk_rows)) in hunks.into_iter().enumerate() {
        // (diff-05) claude-code's StructuredDiffList separates hunks with a dim
        // `...` line between them — it never renders unified `@@ -a,b +c,d @@`
        // headers. The separator is derived from a change in `hunk` at style
        // time, EXCEPT when the cap lands exactly on a hunk boundary: the
        // pre-refactor renderer committed to the separator before testing the
        // cap, so that one case is recorded explicitly to keep bytes identical.
        if hunk > 0 && rows.len() >= max_rows {
            dangling_separator = true;
            truncated = true;
            break 'hunks;
        }
        for row in derive_rows(&hunk_rows, hunk, language.as_deref(), theme) {
            if rows.len() >= max_rows {
                truncated = true;
                break 'hunks;
            }
            rows.push(row);
        }
    }
    let truncated_rows = if truncated {
        total_body - rows.len()
    } else {
        0
    };
    StructuredDiff {
        rows,
        gutter_width,
        additions,
        removals,
        truncated_rows,
        dangling_separator,
        language,
    }
}

/// Lay a [`StructuredDiff`] out as terminal lines: gutter + styled content,
/// dim `...` separators between hunks, and a truncation footer.
///
/// This is the ONLY styling path — `render`/`render_with_width` are thin
/// wrappers over it, so there is exactly one `grouped_hunks`, one
/// `classify_change`, and one `CHANGE_THRESHOLD` decision in the crate.
#[must_use]
pub fn style_structured(diff: &StructuredDiff, theme: ThemeName, width: usize) -> Vec<StyledLine> {
    let mut out: Vec<StyledLine> = Vec::new();
    let mut prev_hunk: Option<usize> = None;
    for row in &diff.rows {
        if prev_hunk.is_some_and(|prev| prev != row.hunk) {
            out.push(hunk_separator());
        }
        prev_hunk = Some(row.hunk);
        let (line_bg, word_bg) = match row.kind {
            LineKind::Add => (add_bg(theme), add_word_bg(theme)),
            LineKind::Remove => (remove_bg(theme), remove_word_bg(theme)),
            LineKind::Context => (StyleColor::Default, StyleColor::Default),
        };
        let mut spans = vec![gutter_span(
            row.kind,
            row.line_no,
            diff.gutter_width,
            line_bg,
            theme,
        )];
        spans.extend(row.segments.iter().map(|seg| {
            StyledSpan::styled(
                seg.text.clone(),
                SpanStyle {
                    fg: seg.fg,
                    bg: if seg.emph { word_bg } else { line_bg },
                    bold: seg.bold,
                    italic: seg.italic,
                    underline: seg.underline,
                },
            )
        }));
        let mut line = StyledLine { spans };
        // (diff-03) Pad changed (Add/Remove) rows so the line bg reaches the
        // right edge; Context keeps the terminal-default bg, so no pad.
        if row.kind != LineKind::Context {
            pad_line_to_width(&mut line, width, line_bg);
        }
        out.push(line);
    }
    if diff.dangling_separator {
        out.push(hunk_separator());
    }
    if diff.truncated_rows > 0 {
        out.push(truncation_footer(diff.truncated_rows));
    }
    out
}

/// (diff-03) Append a space-pad span carrying `bg` so `line`'s total display
/// width reaches `width`. No-op if `line` already reaches/exceeds `width`.
fn pad_line_to_width(line: &mut StyledLine, width: usize, bg: StyleColor) {
    let cur_w: usize = line
        .spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.text.as_str()))
        .sum();
    if cur_w < width {
        line.spans.push(StyledSpan::styled(
            " ".repeat(width - cur_w),
            SpanStyle {
                bg,
                ..SpanStyle::default()
            },
        ));
    }
}

/// Derive a slice of one hunk's diff rows into structured rows, pairing
/// adjacent remove→add runs for word-level diffing (claude-code
/// `processAdjacentLines`).
fn derive_rows(
    rows: &[DiffRow],
    hunk: usize,
    lang: Option<&str>,
    theme: ThemeName,
) -> Vec<StructuredDiffRow> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        if rows[i].kind == LineKind::Remove {
            // Collect the contiguous remove run, then the contiguous add run.
            let rem_start = i;
            while i < rows.len() && rows[i].kind == LineKind::Remove {
                i += 1;
            }
            let rem_end = i;
            let add_start = i;
            while i < rows.len() && rows[i].kind == LineKind::Add {
                i += 1;
            }
            let add_end = i;
            let removes = &rows[rem_start..rem_end];
            let adds = &rows[add_start..add_end];
            // Pair the k-th remove with the k-th add for word diffing; any
            // unpaired surplus on either side falls back to whole-line.
            let pairs = removes.len().min(adds.len());
            for k in 0..pairs {
                let rem = &removes[k];
                let add = &adds[k];
                if let (Some(rem_segments), Some(add_segments)) = (
                    word_diff_segments(&rem.text, &add.text, false, theme),
                    word_diff_segments(&rem.text, &add.text, true, theme),
                ) {
                    out.push(word_structured_row(rem, hunk, rem_segments));
                    out.push(word_structured_row(add, hunk, add_segments));
                } else {
                    // Too dissimilar — whole-line coloring for both.
                    out.push(plain_structured_row(rem, hunk, lang, theme));
                    out.push(plain_structured_row(add, hunk, lang, theme));
                }
            }
            for rem in &removes[pairs..] {
                out.push(plain_structured_row(rem, hunk, lang, theme));
            }
            for add in &adds[pairs..] {
                out.push(plain_structured_row(add, hunk, lang, theme));
            }
        } else {
            out.push(plain_structured_row(&rows[i], hunk, lang, theme));
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a row to its concatenated text for structural assertion.
    fn rowline(l: &StyledLine) -> String {
        l.spans.iter().map(|s| s.text.clone()).collect()
    }

    /// Is this a `@@ ... @@` hunk header line?
    fn is_header(l: &StyledLine) -> bool {
        rowline(l).contains("@@")
    }

    /// Body (non-header) lines only.
    fn body(lines: &[StyledLine]) -> Vec<&StyledLine> {
        lines.iter().filter(|l| !is_header(l)).collect()
    }

    /// Classified rows from the PRODUCTION path (`grouped_hunks`) flattened
    /// across hunks. The `classify_*` tests assert against this so they
    /// validate the same classification the renderer uses — there is now a
    /// single classification code path (`classify_change`).
    fn classified_rows(old: &str, new: &str) -> Vec<DiffRow> {
        grouped_hunks(old, new)
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .collect()
    }

    #[test]
    fn diff_backgrounds_are_half_alpha_of_claude_code_tokens() {
        // EXPERIMENT: diff line/word backgrounds render at 0.5 alpha over black
        // (claude-code diff token * 0.5). Decorations + fg stay opaque.
        assert_eq!(add_bg(ThemeName::Dark), StyleColor::Rgb(17, 46, 21)); // diffAdded*0.5
        assert_eq!(remove_bg(ThemeName::Dark), StyleColor::Rgb(61, 20, 27)); // diffRemoved*0.5
        assert_eq!(add_word_bg(ThemeName::Dark), StyleColor::Rgb(28, 83, 48)); // diffAddedWord*0.5
        assert_eq!(remove_word_bg(ThemeName::Dark), StyleColor::Rgb(89, 44, 53)); // diffRemovedWord*0.5
        assert_eq!(
            add_decoration(ThemeName::Dark),
            StyleColor::Rgb(80, 200, 80)
        );
        assert_eq!(
            remove_decoration(ThemeName::Dark),
            StyleColor::Rgb(220, 90, 90)
        );
        assert_eq!(
            remove_text_fg(ThemeName::Dark),
            StyleColor::Rgb(248, 248, 242)
        );
    }

    #[test]
    fn classify_pure_add() {
        let rows = classified_rows("a\n", "a\nb\n");
        // a = context, b = add
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![LineKind::Context, LineKind::Add]
        );
    }

    #[test]
    fn classify_pure_remove() {
        let rows = classified_rows("a\nb\n", "a\n");
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![LineKind::Context, LineKind::Remove]
        );
    }

    #[test]
    fn classify_modify_is_remove_then_add() {
        let rows = classified_rows("foo\n", "bar\n");
        assert_eq!(
            rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![LineKind::Remove, LineKind::Add]
        );
    }

    #[test]
    fn classify_empty_diff_yields_no_hunks() {
        // No changes -> `grouped_ops` produces no groups -> no rows. (The
        // matching `render` behavior — empty output — is covered by
        // `render_empty_diff_is_empty`.)
        let rows = classified_rows("a\nb\n", "a\nb\n");
        assert!(rows.is_empty());
    }

    #[test]
    fn render_with_width_pads_changed_rows_to_full_width() {
        // (diff-03) A short added line, padded to 30 columns total, carries
        // the green bg on the trailing pad span too.
        let lines = render_with_width("a\n", "a\nb\n", Some("x.txt"), ThemeName::Dark, 30);
        let add = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('+'))
            .expect("an add line");
        let total_w: usize = add
            .spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.text.as_str()))
            .sum();
        assert_eq!(total_w, 30, "row should be padded to the full width");
        let last = add.spans.last().expect("at least one span");
        assert!(
            last.text.chars().all(|c| c == ' '),
            "pad span is spaces: {last:?}"
        );
        assert_eq!(last.style.bg, add_bg(ThemeName::Dark));
    }

    #[test]
    fn render_with_width_does_not_pad_context_rows() {
        // (diff-03) Context rows keep the terminal-default bg — no pad span.
        let lines = render_with_width("a\n", "a\nb\n", Some("x.txt"), ThemeName::Dark, 30);
        let ctx = lines
            .iter()
            .find(|l| !is_header(l) && !rowline(l).contains('+') && !rowline(l).contains('-'))
            .expect("a context line");
        let total_w: usize = ctx
            .spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.text.as_str()))
            .sum();
        assert!(
            total_w < 30,
            "context row must not be padded to width: {total_w}"
        );
    }

    #[test]
    fn render_with_width_zero_matches_render() {
        // width=0 is render()'s exact behavior (no padding ever fires).
        let to_lines = |v: &[StyledLine]| v.iter().map(rowline).collect::<Vec<_>>();
        assert_eq!(
            to_lines(&render_with_width(
                "a\n",
                "a\nb\n",
                Some("x.txt"),
                ThemeName::Dark,
                0
            )),
            to_lines(&render("a\n", "a\nb\n", Some("x.txt"), ThemeName::Dark)),
        );
    }

    #[test]
    fn render_pure_add_has_plus_sigil_and_green_bg() {
        let lines = render("a\n", "a\nb\n", Some("x.txt"), ThemeName::Dark);
        // The add line carries a "+" sigil and the content "b", over green bg.
        let add = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('+'))
            .expect("an add line");
        let joined = rowline(add);
        assert!(joined.contains('b'), "add line content: {joined:?}");
        // At least one span on the add line has the green add background.
        assert!(
            add.spans
                .iter()
                .any(|s| s.style.bg == add_bg(ThemeName::Dark)),
            "add line has green background"
        );
    }

    #[test]
    fn render_pure_remove_has_minus_sigil_and_red_bg() {
        let lines = render("a\nb\n", "a\n", Some("x.txt"), ThemeName::Dark);
        let rem = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('-'))
            .expect("a - line");
        assert!(rowline(rem).contains('b'));
        assert!(
            rem.spans
                .iter()
                .any(|s| s.style.bg == remove_bg(ThemeName::Dark)),
            "remove line has red background"
        );
    }

    #[test]
    fn render_add_gutter_uses_green_decoration_not_dim() {
        // (diff-02) The Add gutter's sigil+number span is colored green, not
        // the dim BrightBlack used for Context lines.
        let lines = render("a\n", "a\nb\n", Some("x.txt"), ThemeName::Dark);
        let add = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('+'))
            .expect("an add line");
        let gutter = &add.spans[0];
        assert_eq!(gutter.style.fg, add_decoration(ThemeName::Dark));
    }

    #[test]
    fn render_remove_gutter_uses_red_decoration_not_dim() {
        // (diff-02)
        let lines = render("a\nb\n", "a\n", Some("x.txt"), ThemeName::Dark);
        let rem = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('-'))
            .expect("a - line");
        let gutter = &rem.spans[0];
        assert_eq!(gutter.style.fg, remove_decoration(ThemeName::Dark));
    }

    #[test]
    fn render_context_gutter_stays_dim() {
        // (diff-02) Context lines keep the BrightBlack dim gutter fg.
        let lines = render("a\n", "a\nb\n", Some("x.txt"), ThemeName::Dark);
        let ctx = lines
            .iter()
            .find(|l| !is_header(l) && !rowline(l).contains('+') && !rowline(l).contains('-'))
            .expect("a context line");
        let gutter = &ctx.spans[0];
        assert_eq!(gutter.style.fg, StyleColor::Named(NamedColor::BrightBlack));
    }

    #[test]
    fn render_gutter_has_line_numbers() {
        let lines = render("a\n", "a\nb\n", Some("x.txt"), ThemeName::Dark);
        let rows = body(&lines);
        // Context line "a" is line 1, add line "b" is line 2.
        assert!(rows.iter().any(|l| rowline(l).contains('1')));
        assert!(rows.iter().any(|l| rowline(l).contains('2')));
    }

    #[test]
    fn diff_stats_counts_additions_and_removals() {
        // (fileedit-result-added-removed-header)
        assert_eq!(diff_stats("a\nb\n", "a\nb\n"), (0, 0));
        assert_eq!(diff_stats("a\n", "a\nb\nc\n"), (2, 0));
        assert_eq!(diff_stats("a\nb\nc\n", "a\n"), (0, 2));
        assert_eq!(diff_stats("a\nb\n", "a\nc\nd\n"), (2, 1));
    }

    #[test]
    fn render_empty_diff_is_empty() {
        // No changes -> no hunks -> empty output (claude-code renders nothing).
        let lines = render("a\nb\n", "a\nb\n", Some("x.txt"), ThemeName::Dark);
        assert!(lines.is_empty());
    }

    #[test]
    fn word_diff_highlights_only_changed_words() {
        // "function oldName(param)" -> "function newName(param)": only the
        // word "oldName"/"newName" should carry the intra-line emphasis bg.
        let lines = render(
            "function oldName(param)\n",
            "function newName(param)\n",
            Some("x.js"),
            ThemeName::Dark,
        );
        // One remove row + one add row (skip the @@ header).
        let rem = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('-'))
            .unwrap();
        let add = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('+'))
            .unwrap();
        // The changed-word span ("oldName"/"newName") carries the EMPHASIS bg,
        // while the unchanged "function "/"(param)" spans carry the line bg.
        assert!(rem
            .spans
            .iter()
            .any(|s| s.text.contains("oldName") && s.style.bg == remove_word_bg(ThemeName::Dark)));
        assert!(add
            .spans
            .iter()
            .any(|s| s.text.contains("newName") && s.style.bg == add_word_bg(ThemeName::Dark)));
        // The shared word "function" is NOT emphasized.
        assert!(add
            .spans
            .iter()
            .any(|s| s.text.contains("function") && s.style.bg != add_word_bg(ThemeName::Dark)));
    }

    #[test]
    fn word_diff_skipped_when_lines_too_dissimilar() {
        // Wildly different lines (> CHANGE_THRESHOLD changed) fall back to
        // whole-line coloring: no word-emphasis spans.
        let lines = render("aaaaaaaa\n", "zzzzzzzz\n", Some("x.txt"), ThemeName::Dark);
        let add = lines
            .iter()
            .find(|l| !is_header(l) && rowline(l).contains('+'))
            .unwrap();
        assert!(
            add.spans
                .iter()
                .all(|s| s.style.bg != add_word_bg(ThemeName::Dark)),
            "dissimilar lines use whole-line coloring, not word emphasis"
        );
    }

    #[test]
    fn render_with_separated_changes_emits_hunk_separator() {
        // Two change clusters separated by a long unchanged run -> the second
        // cluster is preceded by a dim `...` separator. claude-code's
        // StructuredDiffList never renders unified `@@ -a,b +c,d @@` headers.
        let old = (1..=40)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let mut new_lines: Vec<String> = (1..=40).map(|n| format!("line{n}")).collect();
        new_lines[2] = "CHANGED_TOP".into();
        new_lines[37] = "CHANGED_BOTTOM".into();
        let new = new_lines.join("\n") + "\n";
        let lines = render(&old, &new, Some("x.txt"), ThemeName::Dark);
        let separators = lines.iter().filter(|l| rowline(l).trim() == "...").count();
        assert!(
            separators >= 1,
            "separated change clusters are divided by a `...` separator"
        );
        assert!(
            !lines.iter().any(|l| rowline(l).contains("@@")),
            "claude-code never renders unified @@ hunk headers"
        );
    }

    #[test]
    fn render_truncates_large_diff_with_footer() {
        // A diff with > MAX_DIFF_LINES changed lines is truncated with a footer.
        let old = String::new();
        let new = (0..(MAX_DIFF_LINES + 50))
            .map(|n| format!("add{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let lines = render(&old, &new, Some("x.txt"), ThemeName::Dark);
        // Body capped at MAX_DIFF_LINES; total = 1 header + cap + 1 footer.
        assert!(
            lines.len() <= MAX_DIFF_LINES + 2,
            "truncated to header + cap + footer, got {}",
            lines.len()
        );
        let last = rowline(lines.last().unwrap());
        assert!(
            last.contains("more lines"),
            "truncation footer present: {last:?}"
        );
    }

    // ── Structured model ───────────────────────────────────────────────────

    fn structured(old: &str, new: &str, path: Option<&str>) -> StructuredDiff {
        structured_diff(old, new, path, ThemeName::Dark, MAX_DIFF_LINES)
    }

    #[test]
    fn structured_segments_reassemble_each_row_exactly() {
        // The wire ships pre-split segments instead of byte ranges precisely
        // so no consumer has to index a string. That contract only holds if
        // concatenation is lossless — including for multi-byte text.
        let old = "let name = \"旧值\"; // 注释\nfn f() {}\n";
        let new = "let name = \"新值\"; // 注释 🙂\nfn g() {}\n";
        let diff = structured(old, new, Some("x.rs"));
        assert!(!diff.rows.is_empty());
        for row in &diff.rows {
            let reassembled = row.text();
            // Every row's text must appear verbatim in the side it came from.
            let side = match row.kind {
                LineKind::Remove => old,
                LineKind::Add | LineKind::Context => new,
            };
            assert!(
                side.lines().any(|l| l == reassembled),
                "row {reassembled:?} is not a verbatim line of its source side"
            );
        }
    }

    #[test]
    fn structured_marks_word_diffed_pairs_and_emphasizes_changed_words() {
        let diff = structured(
            "function oldName(param)\n",
            "function newName(param)\n",
            Some("x.js"),
        );
        assert!(diff.rows.iter().all(|r| r.word_diffed));
        let add = diff
            .rows
            .iter()
            .find(|r| r.kind == LineKind::Add)
            .expect("an add row");
        // Only the changed token is emphasized. (diff-04) `similar`'s
        // `from_words` does NOT split on `(`, so the token is
        // `newName(param)`, not claude-code's finer `newName`. That divergence
        // is documented on `word_diff_segments` and is inherited verbatim by
        // every client — asserting the TS tokenization here would encode a
        // target this renderer deliberately does not hit.
        let emphasized: Vec<&str> = add
            .segments
            .iter()
            .filter(|s| s.emph)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(emphasized, vec!["newName(param)"]);
        // The shared prefix is NOT emphasized.
        assert!(add
            .segments
            .iter()
            .any(|s| !s.emph && s.text.contains("function")));
        // A word-diffed row carries no syntax classification (claude-code
        // renders word-diffed rows without syntax highlighting).
        assert!(add
            .segments
            .iter()
            .all(|s| s.class == syntax::SyntaxClass::Plain));
    }

    #[test]
    fn structured_falls_back_to_whole_line_when_too_dissimilar() {
        let diff = structured("aaaaaaaa\n", "zzzzzzzz\n", Some("x.txt"));
        assert!(diff.rows.iter().all(|r| !r.word_diffed));
        assert!(diff.rows.iter().flat_map(|r| &r.segments).all(|s| !s.emph));
    }

    #[test]
    fn structured_classifies_added_lines_but_not_removed_ones() {
        // (diff-01) Removed lines render as plaintext — no syntax highlight.
        let diff = structured("fn old_name() {}\n", "fn new_name() {}\n", Some("x.rs"));
        let removed = diff
            .rows
            .iter()
            .find(|r| r.kind == LineKind::Remove)
            .expect("a remove row");
        assert!(removed
            .segments
            .iter()
            .all(|s| s.class == syntax::SyntaxClass::Plain));

        // An unpaired add (no adjacent remove) DOES get classified.
        let diff = structured("a\n", "a\nfn helper() {}\n", Some("x.rs"));
        let added = diff
            .rows
            .iter()
            .find(|r| r.kind == LineKind::Add)
            .expect("an add row");
        assert!(
            added
                .segments
                .iter()
                .any(|s| s.class == syntax::SyntaxClass::Keyword),
            "added Rust line should classify `fn` as a keyword: {:?}",
            added.segments
        );
    }

    #[test]
    fn structured_hunk_index_increments_across_separated_clusters() {
        let old = (1..=40)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let mut new_lines: Vec<String> = (1..=40).map(|n| format!("line{n}")).collect();
        new_lines[2] = "CHANGED_TOP".into();
        new_lines[37] = "CHANGED_BOTTOM".into();
        let new = new_lines.join("\n") + "\n";
        let diff = structured(&old, &new, Some("x.txt"));
        let hunks: Vec<usize> = diff.rows.iter().map(|r| r.hunk).collect();
        assert_eq!(hunks.first(), Some(&0));
        assert_eq!(
            hunks.last(),
            Some(&1),
            "two separated clusters -> two hunks"
        );
        // Hunk indices are non-decreasing.
        assert!(hunks.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn structured_reports_stats_gutter_and_truncation() {
        let new = (0..(MAX_DIFF_LINES + 50))
            .map(|n| format!("add{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let diff = structured("", &new, Some("x.txt"));
        assert_eq!(diff.rows.len(), MAX_DIFF_LINES);
        assert_eq!(diff.truncated_rows, 50);
        // Whole-diff stats, NOT just the emitted rows.
        assert_eq!(diff.additions, MAX_DIFF_LINES + 50);
        assert_eq!(diff.removals, 0);
        // Gutter is sized for the widest line number across the whole diff.
        assert_eq!(diff.gutter_width, (MAX_DIFF_LINES + 50).to_string().len());
        // `detect_language_with_first_line` yields the syntect *token*, which
        // for `x.txt` is the extension itself.
        assert_eq!(diff.language.as_deref(), Some("txt"));
    }

    #[test]
    fn structured_stats_match_diff_stats() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\nd\n";
        let diff = structured(old, new, Some("x.txt"));
        assert_eq!((diff.additions, diff.removals), diff_stats(old, new));
    }

    #[test]
    fn structured_empty_diff_is_empty() {
        let diff = structured("a\nb\n", "a\nb\n", Some("x.txt"));
        assert!(diff.rows.is_empty());
        assert_eq!(diff.truncated_rows, 0);
        assert!(!diff.dangling_separator);
        assert!(style_structured(&diff, ThemeName::Dark, 0).is_empty());
    }

    #[test]
    fn style_structured_is_the_only_path_render_takes() {
        // `render_with_width` must be a pure wrapper: styling the structured
        // diff directly has to produce the identical lines. If these ever
        // diverge, a second styling path has crept back in.
        let old = "fn a() {}\nkeep\nold line\n";
        let new = "fn b() {}\nkeep\nnew line\nextra\n";
        for width in [0usize, 30, 80] {
            let via_wrapper = render_with_width(old, new, Some("x.rs"), ThemeName::Dark, width);
            let via_structured = style_structured(
                &structured_diff(old, new, Some("x.rs"), ThemeName::Dark, MAX_DIFF_LINES),
                ThemeName::Dark,
                width,
            );
            assert_eq!(via_wrapper, via_structured, "width {width}");
        }
    }

    #[test]
    fn wire_row_cap_exceeds_the_terminal_cap() {
        // The `MAX_WIRE_DIFF_ROWS > MAX_DIFF_LINES` invariant is asserted at
        // COMPILE time next to the constants — a runtime `assert!` over two
        // consts is optimized away and can never fail. This test covers the
        // behavior instead: the wire cap is what actually truncates.
        let new = (0..(MAX_WIRE_DIFF_ROWS + 10))
            .map(|n| format!("add{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let diff = structured_diff("", &new, Some("x.txt"), ThemeName::Dark, MAX_WIRE_DIFF_ROWS);
        assert_eq!(diff.rows.len(), MAX_WIRE_DIFF_ROWS);
        assert_eq!(diff.truncated_rows, 10);
    }

    // ── Byte-lock snapshots ────────────────────────────────────────────────
    //
    // These exist so `render`/`render_with_width` can be re-plumbed onto a
    // structured intermediate model without silently moving a single span
    // boundary, color, or gutter character. They snapshot the CURRENT output
    // and must reproduce byte-for-byte afterwards.
    //
    // ⚠️ Never `--accept` these while refactoring the renderer. A diff here is
    // a regression report, not a snapshot that needs updating. Re-blessing is
    // only legitimate when the rendered output is *intended* to change, and
    // then the diff must be read line by line.

    /// Lossless textual fingerprint of rendered lines. Every field of every
    /// [`StyledSpan`] is included (text, fg, bg, bold, italic, underline,
    /// kind), so an `assert_snapshot!` of this locks the renderer's output as
    /// tightly as a YAML dump while staying readable in a review diff.
    fn fingerprint(lines: &[StyledLine]) -> String {
        let mut out = String::new();
        for (i, line) in lines.iter().enumerate() {
            out.push_str(&format!("{i:>3} |"));
            for s in &line.spans {
                out.push_str(&format!(
                    " {:?}[fg={:?} bg={:?}{}{}{}{}]",
                    s.text,
                    s.style.fg,
                    s.style.bg,
                    if s.style.bold { " bold" } else { "" },
                    if s.style.italic { " italic" } else { "" },
                    if s.style.underline { " underline" } else { "" },
                    match &s.kind {
                        crate::render::SpanKind::Text => String::new(),
                        crate::render::SpanKind::CodePlaceholder { lang } =>
                            format!(" code({lang:?})"),
                    },
                ));
            }
            out.push('\n');
        }
        out
    }

    /// Pure add, syntax-highlighted. Locks the add background, the green
    /// gutter decoration, the line-number width, and the syntect span split.
    #[test]
    fn snapshot_lock_pure_add_highlighted() {
        let old = "fn main() {\n}\n";
        let new = "fn main() {\n    let total = compute(1, 2);\n    println!(\"{total}\");\n}\n";
        insta::assert_snapshot!(fingerprint(&render(
            old,
            new,
            Some("main.rs"),
            ThemeName::Dark
        )));
    }

    /// Pure remove. Removed lines render as plaintext (diff-01) over the red
    /// background with the red gutter decoration.
    #[test]
    fn snapshot_lock_pure_remove() {
        let old = "fn main() {\n    let stale = 1;\n    drop(stale);\n}\n";
        let new = "fn main() {\n}\n";
        insta::assert_snapshot!(fingerprint(&render(
            old,
            new,
            Some("main.rs"),
            ThemeName::Dark
        )));
    }

    /// Adjacent remove→add pair below `CHANGE_THRESHOLD`: word-level emphasis
    /// backgrounds on the changed words only, and NO syntect on either row.
    #[test]
    fn snapshot_lock_word_diff_pair() {
        insta::assert_snapshot!(fingerprint(&render(
            "function oldName(param)\n",
            "function newName(param)\n",
            Some("x.js"),
            ThemeName::Dark,
        )));
    }

    /// Adjacent pair ABOVE `CHANGE_THRESHOLD`: word diffing is abandoned and
    /// both rows fall back to whole-line coloring.
    #[test]
    fn snapshot_lock_word_diff_skipped_when_dissimilar() {
        insta::assert_snapshot!(fingerprint(&render(
            "aaaaaaaa\n",
            "zzzzzzzz\n",
            Some("x.txt"),
            ThemeName::Dark,
        )));
    }

    /// Two change clusters separated by a long unchanged run: locks
    /// `CONTEXT_RADIUS`, the hunk grouping, and the dim `...` separator.
    #[test]
    fn snapshot_lock_multi_hunk_separator() {
        let old = (1..=40)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let mut new_lines: Vec<String> = (1..=40).map(|n| format!("line{n}")).collect();
        new_lines[2] = "CHANGED_TOP".into();
        new_lines[37] = "CHANGED_BOTTOM".into();
        let new = new_lines.join("\n") + "\n";
        insta::assert_snapshot!(fingerprint(&render(
            &old,
            &new,
            Some("x.txt"),
            ThemeName::Dark
        )));
    }

    /// `render_with_width` pads changed rows to the full width with the line
    /// background — a distinct code path from `render` (width 0).
    #[test]
    fn snapshot_lock_width_padded_rows() {
        insta::assert_snapshot!(fingerprint(&render_with_width(
            "keep\ndrop me\n",
            "keep\nadded line\n",
            Some("x.txt"),
            ThemeName::Dark,
            30,
        )));
    }

    /// Over-cap diff: locks the row cap and the truncation footer. Only the
    /// head and tail are fingerprinted — per-row styling is already locked by
    /// the fixtures above, and a full 100-row dump would bury the two facts
    /// this fixture exists to pin.
    #[test]
    fn snapshot_lock_truncation_footer() {
        let new = (0..(MAX_DIFF_LINES + 50))
            .map(|n| format!("add{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let lines = render("", &new, Some("x.txt"), ThemeName::Dark);
        let head = fingerprint(&lines[..2]);
        let tail = fingerprint(&lines[lines.len() - 2..]);
        insta::assert_snapshot!(format!(
            "total_lines: {}\n--- head ---\n{head}--- tail ---\n{tail}",
            lines.len()
        ));
    }
}
