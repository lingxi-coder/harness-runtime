//! Markdown TABLE grid layout — bordered, column-aligned grid with a
//! narrow-terminal vertical key:value fallback.
//!
//! Literal reference: `claude-code/src/components/MarkdownTable.tsx`.
//! Constants `SAFETY_MARGIN=4`, `MIN_COLUMN_WIDTH=3`, `MAX_ROW_LINES=4`
//! (`:15-26`); the width algorithm (`:106-156` — min/ideal widths → 3-branch
//! fit: ideal-fits / shrink-to-min+distribute / hard-wrap-scale);
//! `calculateMaxRowLines` (`:159-184`); `renderRowLines` (`:186-223` — per-cell
//! wrap, vertical-center offset, `│`+`padAligned`); `renderBorderLine`
//! (`:226-238` — `┌┬┐` / `├┼┤` / `└┴┘`); `renderVerticalFormat` (`:241-280`).
//! `padAligned` is `claude-code/src/utils/markdown.ts:366`.
//!
//! # 1:1 fidelity divergences (spec A2)
//!   - claude-code measures with its `stringWidth` (npm `string-width`) and
//!     wraps with `wrapAnsi` (npm `wrap-ansi`); this Rust port measures with
//!     [`unicode_width::UnicodeWidthStr`] — the same width primitive the rest of
//!     the TUI uses — and word-wraps with the in-tree [`wrap_text`] helper. For
//!     ASCII/BMP text these agree; exotic emoji/ZWJ widths may differ by a
//!     column.
//!   - claude-code renders the whole table as a single `<Ansi>` block of ANSI
//!     bytes; this port returns [`StyledLine`]s where cell content keeps its
//!     [`StyledSpan`] styling (bold / inline-code color) STRUCTURALLY, not as
//!     literal ANSI. The border/padding cells are plain spans.
//!   - The "safety check" re-measure that claude-code does after building the
//!     grid (re-falling-back to vertical when a built line exceeds
//!     `terminalWidth − SAFETY_MARGIN`) is ported faithfully.

// `claude-code`, `padAligned`, `pulldown-cmark` etc. read better unquoted in
// the module prose; suppress the doc-markdown nudge for this file.
#![allow(clippy::doc_markdown)]

use crate::presentation::render::{SpanStyle, StyledLine, StyledSpan};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Accounts for parent indentation (e.g. message dot prefix) and terminal
/// resize races. Without enough margin the table overflows its layout box.
/// (`MarkdownTable.tsx:15`)
const SAFETY_MARGIN: usize = 4;

/// Minimum column width to prevent degenerate layouts. (`MarkdownTable.tsx:18`)
const MIN_COLUMN_WIDTH: usize = 3;

/// Maximum number of lines per row before switching to vertical format. When
/// wrapping would make rows taller than this, vertical (key-value) format
/// provides better readability. (`MarkdownTable.tsx:25`)
const MAX_ROW_LINES: usize = 4;

/// Maximum number of markdown table body rows to render.
pub const MAX_TABLE_ROWS: usize = 200;

/// Column text alignment, mirroring claude-code's `token.align` entries
/// (`'left' | 'center' | 'right'`, with the markdown default being left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    /// Left-aligned (markdown default).
    Left,
    /// Center-aligned (header cells are always centered).
    Center,
    /// Right-aligned.
    Right,
}

/// Pad `content` to `target_width` according to `align`. `text_w` is the
/// visible display width of `content` (the caller computes it so styling never
/// affects padding). Faithful port of `utils/markdown.ts:366` `padAligned`.
#[must_use]
pub fn pad_aligned(
    content: &str,
    text_w: usize,
    target_width: usize,
    align: ColumnAlign,
) -> String {
    let padding = target_width.saturating_sub(text_w);
    match align {
        ColumnAlign::Center => {
            let left_pad = padding / 2;
            format!(
                "{}{content}{}",
                " ".repeat(left_pad),
                " ".repeat(padding - left_pad)
            )
        }
        ColumnAlign::Right => format!("{}{content}", " ".repeat(padding)),
        ColumnAlign::Left => format!("{content}{}", " ".repeat(padding)),
    }
}

/// Display width of a styled cell's concatenated plain text.
fn cell_plain_text(spans: &[StyledSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// Word-wrap `text` to `width` display columns, returning one entry per line.
///
/// Port of `MarkdownTable.tsx:44-62` `wrapText`. ANSI-strip is unnecessary here
/// (the caller passes plain text). When `hard` is true, words longer than the
/// width are broken at a grapheme boundary (needed when columns are narrower
/// than the longest word). Empty input yields a single empty line so empty
/// cells still occupy a row.
#[must_use]
pub fn wrap_text(text: &str, width: usize, hard: bool) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    // formatToken() adds EOL to paragraphs; trim it so cells gain no blank
    // trailing line (MarkdownTable.tsx:48-51).
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return vec![String::new()];
    }

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;

    // Split on whitespace runs, keeping words; collapse the whitespace itself
    // to single spaces (wrap_ansi with wordWrap behaves this way for the cell
    // content we feed it — cells have no meaningful internal runs of spaces).
    for word in trimmed.split_whitespace() {
        let word_w = UnicodeWidthStr::width(word);
        // A word that does not fit and `hard` is set is broken into pieces.
        if hard && word_w > width {
            // Flush whatever is buffered first.
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            for piece in hard_break_word(word, width) {
                let piece_w = UnicodeWidthStr::width(piece.as_str());
                if current_w > 0 && current_w + 1 + piece_w > width {
                    lines.push(std::mem::take(&mut current));
                    current_w = 0;
                }
                if current.is_empty() {
                    current.push_str(&piece);
                    current_w = piece_w;
                } else {
                    current.push(' ');
                    current.push_str(&piece);
                    current_w += 1 + piece_w;
                }
            }
            continue;
        }

        if current.is_empty() {
            current.push_str(word);
            current_w = word_w;
        } else if current_w + 1 + word_w <= width {
            current.push(' ');
            current.push_str(word);
            current_w += 1 + word_w;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_w = word_w;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Break a single over-long word into width-bounded pieces at grapheme
/// boundaries (display-width aware so wide clusters never straddle the edge).
fn hard_break_word(word: &str, width: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut piece = String::new();
    let mut piece_w = 0usize;
    for g in word.graphemes(true) {
        let gw = UnicodeWidthStr::width(g);
        if piece_w + gw > width && !piece.is_empty() {
            pieces.push(std::mem::take(&mut piece));
            piece_w = 0;
        }
        piece.push_str(g);
        piece_w += gw;
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }
    if pieces.is_empty() {
        pieces.push(String::new());
    }
    pieces
}

/// Longest single-word display width in a cell (the minimum width that avoids
/// breaking a word), floored at [`MIN_COLUMN_WIDTH`]. (`MarkdownTable.tsx:94-99`)
fn cell_min_width(spans: &[StyledSpan]) -> usize {
    let text = cell_plain_text(spans);
    text.split_whitespace()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .max(MIN_COLUMN_WIDTH)
}

/// Ideal width — full content without wrapping, floored at [`MIN_COLUMN_WIDTH`].
/// (`MarkdownTable.tsx:102-104`)
fn cell_ideal_width(spans: &[StyledSpan]) -> usize {
    UnicodeWidthStr::width(cell_plain_text(spans).as_str()).max(MIN_COLUMN_WIDTH)
}

/// Render a markdown table to styled lines, port of `MarkdownTable`'s body.
///
/// `headers` is one styled cell per column; `rows` is one row per data row,
/// each a list of styled cells (a short row's missing trailing cells render
/// empty, matching `row[colIndex]?.tokens`); `aligns` is the per-column
/// alignment (defaulting to [`ColumnAlign::Left`] for columns past its end);
/// `term_width` is the available terminal width; `overflow_rows` is the number
/// of body rows that exceeded [`MAX_TABLE_ROWS`] and were dropped (the caller
/// counts them because the parser stops collecting past the cap) — when > 0 a
/// final `… {N} more {row|rows} not shown` notice is appended, matching
/// claude-code's `Smo()` in every render path; `theme` is currently unused by
/// the layout (cell styling already lives in the spans) but kept for signature
/// parity with the rest of `render::`.
#[must_use]
#[allow(clippy::needless_pass_by_value)] // `theme` mirrors the render:: signature shape.
pub fn render_table(
    headers: &[Vec<StyledSpan>],
    rows: &[Vec<Vec<StyledSpan>>],
    aligns: &[ColumnAlign],
    term_width: usize,
    overflow_rows: usize,
    theme: &super::markdown::MarkdownTheme,
) -> Vec<StyledLine> {
    let _ = theme;
    let rows = &rows[..rows.len().min(MAX_TABLE_ROWS)];
    let num_cols = headers.len();
    if num_cols == 0 {
        return Vec::new();
    }

    // Append the over-cap notice to whichever layout path we return through.
    // (`MarkdownTable.tsx` `Smo()`: the horizontal grid, the vertical `P_s`,
    // and the screen-reader path all push it when the overflow count is > 0.)
    let append_notice = |mut lines: Vec<StyledLine>| -> Vec<StyledLine> {
        if overflow_rows > 0 {
            lines.push(overflow_notice_line(overflow_rows));
        }
        lines
    };

    // Helper: fetch the spans of `row`'s column `col`, or an empty slice.
    let cell_of = |row: &[Vec<StyledSpan>], col: usize| -> Vec<StyledSpan> {
        row.get(col).cloned().unwrap_or_default()
    };

    // Step 1: per-column min (longest word) + ideal (full content) widths.
    // (MarkdownTable.tsx:108-121)
    let mut min_widths = vec![MIN_COLUMN_WIDTH; num_cols];
    let mut ideal_widths = vec![MIN_COLUMN_WIDTH; num_cols];
    for col in 0..num_cols {
        let mut mn = cell_min_width(&headers[col]);
        let mut id = cell_ideal_width(&headers[col]);
        for row in rows {
            let c = cell_of(row, col);
            mn = mn.max(cell_min_width(&c));
            id = id.max(cell_ideal_width(&c));
        }
        min_widths[col] = mn;
        ideal_widths[col] = id;
    }

    // Step 2: available space.
    // Border overhead: │ content │ content │ = 1 + (width + 3) per column.
    // (MarkdownTable.tsx:126-128)
    let border_overhead = 1 + num_cols * 3;
    let available_width = term_width
        .saturating_sub(border_overhead)
        .saturating_sub(SAFETY_MARGIN)
        .max(num_cols * MIN_COLUMN_WIDTH);

    // Step 3: column widths that fit available space. (MarkdownTable.tsx:131-156)
    let total_min: usize = min_widths.iter().sum();
    let total_ideal: usize = ideal_widths.iter().sum();
    let mut needs_hard_wrap = false;
    let column_widths: Vec<usize> = if total_ideal <= available_width {
        // Everything fits — use ideal widths.
        ideal_widths.clone()
    } else if total_min <= available_width {
        // Shrink: give each column its min, distribute remaining space by each
        // column's overflow share (floor division, matching TS Math.floor).
        let extra_space = available_width - total_min;
        let overflows: Vec<usize> = ideal_widths
            .iter()
            .zip(&min_widths)
            .map(|(&ideal, &min)| ideal - min)
            .collect();
        let total_overflow: usize = overflows.iter().sum();
        min_widths
            .iter()
            .enumerate()
            .map(|(i, &min)| {
                if total_overflow == 0 {
                    min
                } else {
                    let extra = overflows[i] * extra_space / total_overflow;
                    min + extra
                }
            })
            .collect()
    } else {
        // Table wider than terminal at minimum widths: shrink proportionally,
        // allowing word breaks. (TS uses float scaleFactor + Math.floor.)
        needs_hard_wrap = true;
        min_widths
            .iter()
            .map(|&w| {
                // floor(w * available / total_min), floored at MIN_COLUMN_WIDTH.
                ((w * available_width) / total_min).max(MIN_COLUMN_WIDTH)
            })
            .collect()
    };

    // Step 4: max row lines → vertical fallback. (MarkdownTable.tsx:159-184)
    let max_row_lines = calculate_max_row_lines(headers, rows, &column_widths, needs_hard_wrap);
    let use_vertical_format = max_row_lines > MAX_ROW_LINES;

    if use_vertical_format {
        return append_notice(render_vertical_format(headers, rows, term_width));
    }

    // Build the complete horizontal table. (MarkdownTable.tsx:294-307)
    let mut table_lines: Vec<StyledLine> = Vec::new();
    table_lines.push(render_border_line(BorderKind::Top, &column_widths));
    table_lines.extend(render_row_lines(
        headers,
        true,
        &column_widths,
        aligns,
        needs_hard_wrap,
    ));
    table_lines.push(render_border_line(BorderKind::Middle, &column_widths));
    for (row_index, row) in rows.iter().enumerate() {
        let cells: Vec<Vec<StyledSpan>> = (0..num_cols).map(|c| cell_of(row, c)).collect();
        table_lines.extend(render_row_lines(
            &cells,
            false,
            &column_widths,
            aligns,
            needs_hard_wrap,
        ));
        if row_index < rows.len() - 1 {
            table_lines.push(render_border_line(BorderKind::Middle, &column_widths));
        }
    }
    table_lines.push(render_border_line(BorderKind::Bottom, &column_widths));

    // Safety check: re-fall-back to vertical when any built line exceeds
    // `terminalWidth − SAFETY_MARGIN`. (MarkdownTable.tsx:310-322)
    let max_line_width = table_lines
        .iter()
        .map(|l| UnicodeWidthStr::width(l.plain_text().as_str()))
        .max()
        .unwrap_or(0);
    if max_line_width > term_width.saturating_sub(SAFETY_MARGIN) {
        return append_notice(render_vertical_format(headers, rows, term_width));
    }

    append_notice(table_lines)
}

/// Build the over-cap notice line, a faithful port of claude-code's
/// `Smo(e)` (`function Smo(e){return`… ${e.toLocaleString()} more
/// ${qt(e,"row")} not shown`}`): a leading `…`, the dropped-row count with
/// en-US thousands separators, and a pluralized `row`/`rows`.
fn overflow_notice_line(overflow_rows: usize) -> StyledLine {
    let noun = if overflow_rows == 1 { "row" } else { "rows" };
    StyledLine::plain(format!(
        "… {} more {noun} not shown",
        to_locale_string(overflow_rows as u64)
    ))
}

/// `Number.prototype.toLocaleString()` for a nonnegative integer under the
/// en-US locale: comma thousands separators (`1234567 -> "1,234,567"`).
fn to_locale_string(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len.saturating_sub(1) / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Compute the maximum wrapped-line count across header + data cells.
/// (`MarkdownTable.tsx:159-184`)
fn calculate_max_row_lines(
    headers: &[Vec<StyledSpan>],
    rows: &[Vec<Vec<StyledSpan>>],
    column_widths: &[usize],
    needs_hard_wrap: bool,
) -> usize {
    let mut max_lines = 1usize;
    for (i, cell) in headers.iter().enumerate() {
        let w = column_widths.get(i).copied().unwrap_or(MIN_COLUMN_WIDTH);
        let wrapped = wrap_text(&cell_plain_text(cell), w, needs_hard_wrap);
        max_lines = max_lines.max(wrapped.len());
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            let w = column_widths.get(i).copied().unwrap_or(MIN_COLUMN_WIDTH);
            let wrapped = wrap_text(&cell_plain_text(cell), w, needs_hard_wrap);
            max_lines = max_lines.max(wrapped.len());
        }
    }
    max_lines
}

/// Wrap a styled cell into per-visual-line span groups at `width`, preserving
/// per-span style. Built on the plain-text [`wrap_text`] (so wrapping matches
/// the width algorithm exactly), then re-applying styles by re-walking the
/// cell's spans across the wrapped plain text.
fn wrap_cell_styled(spans: &[StyledSpan], width: usize, hard: bool) -> Vec<Vec<StyledSpan>> {
    let plain = cell_plain_text(spans);
    let wrapped = wrap_text(&plain, width, hard);
    // Single uniform style? (the common case: a cell is one span, or all spans
    // share a style) — emit one span per line carrying that style.
    let uniform_style: Option<SpanStyle> = match spans.split_first() {
        None => Some(SpanStyle::default()),
        Some((first, rest)) if rest.iter().all(|s| s.style == first.style) => Some(first.style),
        _ => None,
    };
    if let Some(style) = uniform_style {
        return wrapped
            .into_iter()
            .map(|line| {
                if line.is_empty() {
                    Vec::new()
                } else {
                    vec![StyledSpan::styled(line, style)]
                }
            })
            .collect();
    }
    // Mixed styles: word-wrap rebuilds plain text by collapsing internal
    // whitespace, so an exact span-offset remap is lossy. Faithfully preserve
    // STRUCTURE by mapping each wrapped line back onto the cell's style runs via
    // a running grapheme cursor over the original concatenated text.
    remap_styles_onto_lines(spans, &wrapped)
}

/// Map wrapped plain-text `lines` back onto the cell's per-span styles by
/// matching graphemes in order. Whitespace re-inserted by the wrapper that was
/// not in the original carries the default style.
fn remap_styles_onto_lines(spans: &[StyledSpan], lines: &[String]) -> Vec<Vec<StyledSpan>> {
    // Flatten the cell into (grapheme, style) pairs, skipping whitespace (the
    // wrapper normalizes whitespace) so we can re-attach styles to non-space
    // graphemes of each wrapped line in order.
    let mut styled_graphemes: Vec<(String, SpanStyle)> = Vec::new();
    for s in spans {
        for g in s.text.graphemes(true) {
            if !g.chars().all(char::is_whitespace) {
                styled_graphemes.push((g.to_string(), s.style));
            }
        }
    }
    let mut cursor = 0usize;
    let mut out: Vec<Vec<StyledSpan>> = Vec::new();
    for line in lines {
        let mut spans_for_line: Vec<StyledSpan> = Vec::new();
        let mut buf = String::new();
        let mut buf_style = SpanStyle::default();
        let mut buf_has = false;
        for g in line.graphemes(true) {
            let style = if g.chars().all(char::is_whitespace) {
                SpanStyle::default()
            } else {
                let st = styled_graphemes
                    .get(cursor)
                    .map_or(SpanStyle::default(), |(_, st)| *st);
                cursor += 1;
                st
            };
            if buf_has && style == buf_style {
                buf.push_str(g);
            } else {
                if buf_has {
                    spans_for_line.push(StyledSpan::styled(std::mem::take(&mut buf), buf_style));
                }
                buf.push_str(g);
                buf_style = style;
                buf_has = true;
            }
        }
        if buf_has {
            spans_for_line.push(StyledSpan::styled(buf, buf_style));
        }
        out.push(spans_for_line);
    }
    out
}

/// Render a single (header or data) row to one or more styled lines, with
/// per-cell wrapping and vertical-centering. (`MarkdownTable.tsx:186-223`)
fn render_row_lines(
    cells: &[Vec<StyledSpan>],
    is_header: bool,
    column_widths: &[usize],
    aligns: &[ColumnAlign],
    needs_hard_wrap: bool,
) -> Vec<StyledLine> {
    let cell_lines: Vec<Vec<Vec<StyledSpan>>> = cells
        .iter()
        .enumerate()
        .map(|(col, cell)| {
            let w = column_widths.get(col).copied().unwrap_or(MIN_COLUMN_WIDTH);
            wrap_cell_styled(cell, w, needs_hard_wrap)
        })
        .collect();

    let max_lines = cell_lines.iter().map(Vec::len).max().unwrap_or(1).max(1);
    // Vertical offset to center each cell's content. (MarkdownTable.tsx:204)
    let vertical_offsets: Vec<usize> = cell_lines
        .iter()
        .map(|lines| (max_lines - lines.len()) / 2)
        .collect();

    let mut result: Vec<StyledLine> = Vec::new();
    for line_idx in 0..max_lines {
        let mut spans: Vec<StyledSpan> = vec![StyledSpan::plain("│")];
        for col in 0..cells.len() {
            let lines = &cell_lines[col];
            let offset = vertical_offsets[col];
            let content_line: &[StyledSpan] = line_idx
                .checked_sub(offset)
                .and_then(|idx| lines.get(idx))
                .map_or(&[], Vec::as_slice);
            let width = column_widths.get(col).copied().unwrap_or(MIN_COLUMN_WIDTH);
            // Headers always centered; data uses table alignment.
            // (MarkdownTable.tsx:217)
            let align = if is_header {
                ColumnAlign::Center
            } else {
                aligns.get(col).copied().unwrap_or(ColumnAlign::Left)
            };
            let line_w: usize = content_line
                .iter()
                .map(|s| UnicodeWidthStr::width(s.text.as_str()))
                .sum();
            // ` ` + padAligned(content) + ` │`. Padding is plain; content keeps
            // its per-span styling.
            let padding = width.saturating_sub(line_w);
            spans.push(StyledSpan::plain(" "));
            match align {
                ColumnAlign::Center => {
                    let left_pad = padding / 2;
                    if left_pad > 0 {
                        spans.push(StyledSpan::plain(" ".repeat(left_pad)));
                    }
                    spans.extend(content_line.iter().cloned());
                    let right_pad = padding - left_pad;
                    if right_pad > 0 {
                        spans.push(StyledSpan::plain(" ".repeat(right_pad)));
                    }
                }
                ColumnAlign::Right => {
                    if padding > 0 {
                        spans.push(StyledSpan::plain(" ".repeat(padding)));
                    }
                    spans.extend(content_line.iter().cloned());
                }
                ColumnAlign::Left => {
                    spans.extend(content_line.iter().cloned());
                    if padding > 0 {
                        spans.push(StyledSpan::plain(" ".repeat(padding)));
                    }
                }
            }
            spans.push(StyledSpan::plain(" │"));
        }
        result.push(StyledLine { spans });
    }
    result
}

/// Which horizontal border to draw.
#[derive(Clone, Copy)]
enum BorderKind {
    Top,
    Middle,
    Bottom,
}

/// Render a horizontal border line. (`MarkdownTable.tsx:226-238`)
fn render_border_line(kind: BorderKind, column_widths: &[usize]) -> StyledLine {
    let (left, mid, cross, right) = match kind {
        BorderKind::Top => ('┌', '─', '┬', '┐'),
        BorderKind::Middle => ('├', '─', '┼', '┤'),
        BorderKind::Bottom => ('└', '─', '┴', '┘'),
    };
    let mut line = String::new();
    line.push(left);
    let last = column_widths.len().saturating_sub(1);
    for (col, &width) in column_widths.iter().enumerate() {
        line.push_str(&mid.to_string().repeat(width + 2));
        line.push(if col < last { cross } else { right });
    }
    StyledLine::plain(line)
}

/// ANSI bold open/close used around vertical-format labels. We carry the bold
/// STRUCTURALLY on the [`StyledSpan`] rather than as literal ANSI bytes.
/// (`MarkdownTable.tsx:28-29` `ANSI_BOLD_START`/`ANSI_BOLD_END`)
fn bold_style() -> SpanStyle {
    SpanStyle {
        bold: true,
        ..SpanStyle::default()
    }
}

/// Render the vertical (key:value) format for extra-narrow terminals.
/// (`MarkdownTable.tsx:241-280`)
///
/// cc 2.1.198 overflow clamp ("Fixed markdown tables overflowing the right
/// border in fullscreen", changelog 2.1.198): every emitted line is clamped to
/// `term_width − SAFETY_MARGIN` — the same invariant the grid format's safety
/// check enforces. The pre-fix layout let three things escape the frame: a
/// label wider than the frame was never wrapped, the first-line value width
/// was floored at 10 columns even when the label left no room, and unbroken
/// over-long words (URLs) were kept intact past the right border.
fn render_vertical_format(
    headers: &[Vec<StyledSpan>],
    rows: &[Vec<Vec<StyledSpan>>],
    term_width: usize,
) -> Vec<StyledLine> {
    // Small indent for wrapped lines (just 2 spaces). (MarkdownTable.tsx:247)
    const WRAP_INDENT: &str = "  ";

    // The frame every vertical line must fit in (cc 2.1.198 overflow clamp;
    // SAFETY_MARGIN covers parent indentation like the message dot prefix).
    let avail = term_width
        .saturating_sub(SAFETY_MARGIN)
        .max(MIN_COLUMN_WIDTH);

    let mut lines: Vec<StyledLine> = Vec::new();
    let header_texts: Vec<String> = headers.iter().map(|h| cell_plain_text(h)).collect();
    let separator_width = avail.min(40);
    let separator: String = "─".repeat(separator_width);

    for (row_index, row) in rows.iter().enumerate() {
        if row_index > 0 {
            lines.push(StyledLine::plain(separator.clone()));
        }
        for (col, cell) in row.iter().enumerate() {
            let label = header_texts
                .get(col)
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Column {}", col + 1));
            // Clean value: trim, collapse internal whitespace/newlines to single
            // spaces. (MarkdownTable.tsx:255-256)
            let raw_value = cell_plain_text(cell);
            let value = collapse_whitespace(raw_value.trim_end());

            let label_w = UnicodeWidthStr::width(label.as_str());
            let subsequent_line_width = avail.saturating_sub(WRAP_INDENT.len());

            // A label that leaves no room for a value on its own line gets
            // hard-broken to the frame; the value then starts on continuation
            // lines instead of overflowing to the right (2.1.198 clamp).
            // `label:` + ` ` must leave at least MIN_COLUMN_WIDTH for a value.
            let first_line_width = avail.saturating_sub(label_w).saturating_sub(2);
            if first_line_width < MIN_COLUMN_WIDTH {
                let label_line = format!("{label}:");
                for piece in hard_break_word(&label_line, avail) {
                    lines.push(StyledLine {
                        spans: vec![StyledSpan::styled(piece, bold_style())],
                    });
                }
                for line in wrap_text(&value, subsequent_line_width.max(1), true) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    lines.push(StyledLine::plain(format!("{WRAP_INDENT}{line}")));
                }
                continue;
            }

            // Two-pass wrap: first line narrower, continuation lines wider.
            // Hard-break over-long words so no line escapes the frame.
            // (MarkdownTable.tsx:264-274 + 2.1.198 clamp)
            let first_pass = wrap_text(&value, first_line_width, true);
            let first_line = first_pass.first().cloned().unwrap_or_default();
            let wrapped_value: Vec<String> =
                if first_pass.len() <= 1 || subsequent_line_width <= first_line_width {
                    first_pass
                } else {
                    let remaining: String = first_pass[1..]
                        .iter()
                        .map(|l| l.trim())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let rewrapped = wrap_text(&remaining, subsequent_line_width, true);
                    let mut v = vec![first_line.clone()];
                    v.extend(rewrapped);
                    v
                };

            // First line: bold label + value. (MarkdownTable.tsx:277)
            lines.push(StyledLine {
                spans: vec![
                    StyledSpan::styled(format!("{label}:"), bold_style()),
                    StyledSpan::plain(format!(
                        " {}",
                        wrapped_value.first().cloned().unwrap_or_default()
                    )),
                ],
            });
            // Subsequent lines with small indent (skip empty). (MarkdownTable.tsx:280-285)
            for line in wrapped_value.iter().skip(1) {
                if line.trim().is_empty() {
                    continue;
                }
                lines.push(StyledLine::plain(format!("{WRAP_INDENT}{line}")));
            }
        }
    }
    lines
}

/// Collapse all runs of whitespace (incl. newlines) to single spaces and trim.
/// Port of the `value.replace(/\n+/g,' ').replace(/\s+/g,' ').trim()` chain
/// (`MarkdownTable.tsx:256`).
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str) -> Vec<StyledSpan> {
        vec![StyledSpan::plain(text)]
    }

    fn header(cols: &[&str]) -> Vec<Vec<StyledSpan>> {
        cols.iter().map(|c| cell(c)).collect()
    }

    fn body(rows: &[&[&str]]) -> Vec<Vec<Vec<StyledSpan>>> {
        rows.iter()
            .map(|r| r.iter().map(|c| cell(c)).collect())
            .collect()
    }

    fn theme() -> super::super::markdown::MarkdownTheme {
        super::super::markdown::MarkdownTheme {
            inline_code: crate::presentation::render::StyleColor::Default,
            code_theme: crate::presentation::theme::ThemeName::Dark,
        }
    }

    fn plain_lines(lines: &[StyledLine]) -> Vec<String> {
        lines.iter().map(StyledLine::plain_text).collect()
    }

    #[test]
    fn pad_aligned_left_center_right() {
        assert_eq!(pad_aligned("ab", 2, 6, ColumnAlign::Left), "ab    ");
        assert_eq!(pad_aligned("ab", 2, 6, ColumnAlign::Right), "    ab");
        // padding 4 → 2 left, 2 right.
        assert_eq!(pad_aligned("ab", 2, 6, ColumnAlign::Center), "  ab  ");
        // odd padding → floor on the left.
        assert_eq!(pad_aligned("ab", 2, 5, ColumnAlign::Center), " ab  ");
        // text wider than target → no padding (saturating).
        assert_eq!(pad_aligned("abcdef", 6, 3, ColumnAlign::Left), "abcdef");
    }

    #[test]
    fn wrap_text_basic_word_wrap() {
        assert_eq!(wrap_text("hello world", 5, false), vec!["hello", "world"]);
        assert_eq!(wrap_text("hello world", 11, false), vec!["hello world"]);
        // empty cell → single empty line.
        assert_eq!(wrap_text("", 5, false), vec![String::new()]);
    }

    #[test]
    fn wrap_text_hard_breaks_long_words() {
        // "abcdefgh" longer than width 4 → broken into pieces of width 4.
        assert_eq!(wrap_text("abcdefgh", 4, true), vec!["abcd", "efgh"]);
        // without hard wrap, an over-long word is left intact on its own line.
        assert_eq!(wrap_text("abcdefgh", 4, false), vec!["abcdefgh"]);
    }

    #[test]
    fn simple_two_col_fits_ideal() {
        let lines = render_table(
            &header(&["A", "B"]),
            &body(&[&["1", "2"]]),
            &[ColumnAlign::Left, ColumnAlign::Left],
            80,
            0,
            &theme(),
        );
        let plain = plain_lines(&lines);
        // top, header, middle, row, bottom = 5 lines.
        assert_eq!(plain.len(), 5);
        assert!(plain[0].starts_with('┌') && plain[0].ends_with('┐'));
        assert!(plain[2].starts_with('├') && plain[2].contains('┼') && plain[2].ends_with('┤'));
        assert!(plain[4].starts_with('└') && plain[4].ends_with('┘'));
        // header centered (each col MIN_COLUMN_WIDTH=3): "│  A  │  B  │".
        assert_eq!(plain[1], "│  A  │  B  │");
        // data left-aligned: "│ 1   │ 2   │".
        assert_eq!(plain[3], "│ 1   │ 2   │");
    }

    #[test]
    fn alignment_left_center_right() {
        let lines = render_table(
            &header(&["L", "C", "R"]),
            &body(&[&["a", "b", "c"]]),
            &[ColumnAlign::Left, ColumnAlign::Center, ColumnAlign::Right],
            80,
            0,
            &theme(),
        );
        let plain = plain_lines(&lines);
        // data row: left → "a  ", center → " b ", right → "  c".
        assert_eq!(plain[3], "│ a   │  b  │   c │");
    }

    #[test]
    fn shrink_to_min_distribution() {
        // Two columns whose ideal widths exceed a narrow terminal but whose
        // min (longest word) widths still fit → distribute branch.
        let lines = render_table(
            &header(&["col one heading", "col two heading"]),
            &body(&[&["alpha beta gamma", "delta epsilon zeta"]]),
            &[ColumnAlign::Left, ColumnAlign::Left],
            40,
            0,
            &theme(),
        );
        let plain = plain_lines(&lines);
        // No line may exceed the terminal width.
        for l in &plain {
            assert!(
                UnicodeWidthStr::width(l.as_str()) <= 40,
                "line too wide: {l:?}"
            );
        }
        // Still a bordered grid (not vertical fallback).
        assert!(plain[0].starts_with('┌'));
    }

    #[test]
    fn hard_wrap_scale_very_narrow() {
        // A very long single word forces the hard-wrap-scale branch.
        let lines = render_table(
            &header(&["Description"]),
            &body(&[&["supercalifragilisticexpialidocious"]]),
            &[ColumnAlign::Left],
            20,
            0,
            &theme(),
        );
        let plain = plain_lines(&lines);
        // Either grid (hard-wrapped) or vertical fallback — both must respect
        // the safety margin width.
        for l in &plain {
            assert!(
                UnicodeWidthStr::width(l.as_str()) <= 20,
                "line too wide: {l:?}"
            );
        }
    }

    #[test]
    fn vertical_fallback_when_rows_too_tall() {
        // Force >4 wrapped lines per row at a narrow width → vertical format.
        let long = "one two three four five six seven eight nine ten eleven twelve";
        let lines = render_table(
            &header(&["Field"]),
            &body(&[&[long]]),
            &[ColumnAlign::Left],
            16,
            0,
            &theme(),
        );
        let plain = plain_lines(&lines);
        // Vertical format has NO box-drawing corners.
        assert!(!plain.iter().any(|l| l.contains('┌') || l.contains('┼')));
        // The label appears with a colon.
        assert!(plain.iter().any(|l| l.starts_with("Field:")));
    }

    #[test]
    fn border_glyphs_top_middle_bottom() {
        let widths = vec![3usize, 4usize];
        let top = render_border_line(BorderKind::Top, &widths).plain_text();
        let mid = render_border_line(BorderKind::Middle, &widths).plain_text();
        let bot = render_border_line(BorderKind::Bottom, &widths).plain_text();
        assert_eq!(top, "┌─────┬──────┐");
        assert_eq!(mid, "├─────┼──────┤");
        assert_eq!(bot, "└─────┴──────┘");
    }

    #[test]
    fn short_row_renders_missing_cells_empty() {
        // A row with fewer cells than headers must not panic; missing cells
        // render empty.
        let lines = render_table(
            &header(&["A", "B"]),
            &body(&[&["1"]]),
            &[ColumnAlign::Left, ColumnAlign::Left],
            80,
            0,
            &theme(),
        );
        let plain = plain_lines(&lines);
        assert_eq!(plain[3], "│ 1   │     │");
    }

    #[test]
    fn styled_cell_preserves_span_style() {
        // A bold cell keeps its bold styling structurally on the content span.
        let bold = vec![StyledSpan::styled("bold", bold_style())];
        let headers = vec![cell("H")];
        let rows = vec![vec![bold]];
        let lines = render_table(&headers, &rows, &[ColumnAlign::Left], 80, 0, &theme());
        // Find the span carrying "bold" and assert it is bold.
        let span = lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.text == "bold")
            .expect("bold content span present");
        assert!(span.style.bold);
    }

    #[test]
    fn empty_table_is_no_lines() {
        let lines = render_table(&[], &[], &[], 80, 0, &theme());
        assert!(lines.is_empty());
    }

    #[test]
    fn table_body_rows_are_capped_at_200() {
        let headers = header(&["id"]);
        let rows: Vec<Vec<Vec<StyledSpan>>> = (0..=MAX_TABLE_ROWS)
            .map(|idx| vec![cell(&format!("row-{idx}"))])
            .collect();
        let lines = render_table(&headers, &rows, &[ColumnAlign::Left], 80, 0, &theme());
        let plain = plain_lines(&lines).join("\n");
        assert!(plain.contains("row-199"));
        assert!(!plain.contains("row-200"));
    }

    #[test]
    fn to_locale_string_groups_thousands() {
        assert_eq!(to_locale_string(0), "0");
        assert_eq!(to_locale_string(999), "999");
        assert_eq!(to_locale_string(1_000), "1,000");
        assert_eq!(to_locale_string(1_234_567), "1,234,567");
    }

    #[test]
    fn overflow_notice_pluralizes_and_groups() {
        // Singular for exactly one dropped row.
        assert_eq!(
            overflow_notice_line(1).plain_text(),
            "… 1 more row not shown"
        );
        // Plural + thousands separators for larger counts.
        assert_eq!(
            overflow_notice_line(1_300).plain_text(),
            "… 1,300 more rows not shown"
        );
    }

    #[test]
    fn grid_appends_over_cap_notice() {
        // With a positive overflow count the horizontal grid ends with the
        // `… N more rows not shown` notice after the bottom border.
        let lines = render_table(
            &header(&["id"]),
            &body(&[&["1"], &["2"]]),
            &[ColumnAlign::Left],
            80,
            5,
            &theme(),
        );
        let plain = plain_lines(&lines);
        assert!(plain[plain.len() - 2].starts_with('└'));
        assert_eq!(plain.last().unwrap(), "… 5 more rows not shown");
    }

    #[test]
    fn grid_no_notice_when_no_overflow() {
        let lines = render_table(
            &header(&["id"]),
            &body(&[&["1"]]),
            &[ColumnAlign::Left],
            80,
            0,
            &theme(),
        );
        assert!(!plain_lines(&lines).iter().any(|l| l.contains("not shown")));
    }

    #[test]
    fn vertical_fallback_appends_over_cap_notice() {
        // Force the vertical fallback (rows too tall at a narrow width); the
        // notice must still be appended as the final line.
        let long = "one two three four five six seven eight nine ten eleven twelve";
        let lines = render_table(
            &header(&["Field"]),
            &body(&[&[long]]),
            &[ColumnAlign::Left],
            16,
            42,
            &theme(),
        );
        let plain = plain_lines(&lines);
        // Vertical fallback (no box corners) and notice last.
        assert!(!plain.iter().any(|l| l.contains('┌')));
        assert_eq!(plain.last().unwrap(), "… 42 more rows not shown");
    }

    /// Assert the cc 2.1.198 overflow clamp: no rendered line may exceed the
    /// frame ("Fixed markdown tables overflowing the right border in
    /// fullscreen", changelog 2.1.198).
    fn assert_fits(lines: &[StyledLine], term_width: usize) {
        for l in plain_lines(lines) {
            assert!(
                UnicodeWidthStr::width(l.as_str()) <= term_width,
                "line overflows {term_width}-col frame: {l:?}"
            );
        }
    }

    #[test]
    fn vertical_fallback_long_label_does_not_overflow_frame() {
        // A header label as wide as the frame used to render un-wrapped past
        // the right border (first-line value width was floored at 10 columns).
        let long_row = "one two three four five six seven eight nine ten";
        for width in [12usize, 16, 20] {
            let lines = render_table(
                &header(&["ConfigurationKeyName"]),
                &body(&[&[long_row]]),
                &[ColumnAlign::Left],
                width,
                0,
                &theme(),
            );
            assert_fits(&lines, width);
            // Sanity: this exercises the vertical fallback, not the grid.
            assert!(!plain_lines(&lines).iter().any(|l| l.contains('┌')));
        }
    }

    #[test]
    fn vertical_fallback_long_word_value_hard_breaks_to_frame() {
        // An unbroken over-long word (e.g. a URL) used to escape the frame
        // because the vertical format wrapped without hard word breaks.
        let url = "https://example.com/some/very/long/path/that/never/ends/at/all";
        let tall = "a b c d e f g h i j k l m n o p q r s t u v w x y z";
        for width in [16usize, 24, 32] {
            let lines = render_table(
                &header(&["Link", "Notes"]),
                &body(&[&[url, tall]]),
                &[ColumnAlign::Left, ColumnAlign::Left],
                width,
                0,
                &theme(),
            );
            assert_fits(&lines, width);
        }
    }

    #[test]
    fn grid_and_fallback_fit_frame_across_widths() {
        // Sweep widths across both layout branches; the safety invariant must
        // hold everywhere (lines ≤ term_width − SAFETY_MARGIN covers the
        // message renderer's 2-column indent on top).
        let headers = header(&["Name", "Description", "Status"]);
        let rows = body(&[
            &["alpha", "does a thing with words", "ok"],
            &["beta", "supercalifragilisticexpialidocious", "pending"],
        ]);
        for width in 8usize..=100 {
            let lines = render_table(
                &headers,
                &rows,
                &[ColumnAlign::Left, ColumnAlign::Left, ColumnAlign::Right],
                width,
                0,
                &theme(),
            );
            assert_fits(&lines, width);
        }
    }
}
