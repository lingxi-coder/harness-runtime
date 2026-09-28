//! `structuredPatch` hunk array — a 1:1 port of the `jsdiff` (npm `diff`)
//! `structuredPatch` function that claude-code's `FileEditTool` / `FileWriteTool`
//! emit as `data.structuredPatch` (2.1.191). Each hunk is
//! `{oldStart, oldLines, newStart, newLines, lines}` where `lines` carry the
//! ` `/`+`/`-` prefix and NO trailing newline (jsdiff strips it in a final pass,
//! inserting `\ No newline at end of file` for a line that lacked one).
//!
//! The algorithm is byte-faithful to jsdiff's loop (extracted from the live
//! binary): context defaults to **4**, 1-based line counters, an unchanged gap of
//! `<= 2*context` lines (and not within the last two segments) is merged into the
//! current hunk as context, otherwise the hunk closes with `min(gap, context)`
//! trailing context lines. The underlying line diff is Myers (jsdiff `diffLines`
//! ≈ `similar::TextDiff::from_lines`).
//!
//! NOTE: a *create* (empty `before`) emits `[]` at the tool layer (claude returns
//! `structuredPatch: []` for new files) — callers handle that before calling here.

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

/// jsdiff `structuredPatch` default `options.context`.
const PATCH_CONTEXT: i64 = 4;

/// The `{context:8}` claude-code's changed-files differ passes
/// (2.1.238 `SEf` @289903016:
/// `F2t("file.txt","file.txt",e,t,void 0,void 0,{context:8,timeout:JEi})`).
pub const CHANGED_FILE_PATCH_CONTEXT: i64 = 8;

/// One hunk of a [`build_structured_patch`] result. Serializes to the binary's
/// camelCase keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StructuredPatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: i64,
    #[serde(rename = "oldLines")]
    pub old_lines: i64,
    #[serde(rename = "newStart")]
    pub new_start: i64,
    #[serde(rename = "newLines")]
    pub new_lines: i64,
    pub lines: Vec<String>,
}

pub(crate) fn count_patch_lines(hunks: &[StructuredPatchHunk]) -> (u64, u64) {
    let mut added = 0_u64;
    let mut removed = 0_u64;
    for hunk in hunks {
        for line in &hunk.lines {
            if line.starts_with('+') {
                added += 1;
            } else if line.starts_with('-') {
                removed += 1;
            }
        }
    }
    (added, removed)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum SegKind {
    Unchanged,
    Removed,
    Added,
}

struct Seg {
    kind: SegKind,
    /// Each line value WITH its trailing `\n` (or without it for a final line that
    /// lacks one — `similar` reports that via `missing_newline()`).
    lines: Vec<String>,
}

/// Build the `structuredPatch` hunk array for `before` → `after`, 1:1 with jsdiff,
/// at jsdiff's DEFAULT context of 4 — the value the Edit/Write tools'
/// `data.structuredPatch` is built with.
#[must_use]
pub fn build_structured_patch(before: &str, after: &str) -> Vec<StructuredPatchHunk> {
    build_structured_patch_with_context(before, after, PATCH_CONTEXT)
}

/// Build the `structuredPatch` hunk array with an explicit `options.context`.
///
/// Additive: `build_structured_patch` is this function at jsdiff's default 4.
/// The changed-files (`edited_text_file`) reminder needs **8**
/// ([`CHANGED_FILE_PATCH_CONTEXT`]) — 2.1.238 `SEf` @289903016 calls
/// `structuredPatch(..., {context:8})`.
///
/// `context` is clamped at 0; jsdiff treats a negative context as 0 through its
/// `Math.min`/slice arithmetic.
#[must_use]
pub fn build_structured_patch_with_context(
    before: &str,
    after: &str,
    context: i64,
) -> Vec<StructuredPatchHunk> {
    let patch_context = context.max(0);
    // 1. Group the line-level diff into jsdiff-style segments (runs of the same
    //    tag). For a replacement, `similar` emits the deleted lines then the
    //    inserted lines, matching jsdiff's removed-before-added ordering.
    let diff = TextDiff::from_lines(before, after);
    let mut segs: Vec<Seg> = Vec::new();
    for ch in diff.iter_all_changes() {
        let kind = match ch.tag() {
            ChangeTag::Equal => SegKind::Unchanged,
            ChangeTag::Delete => SegKind::Removed,
            ChangeTag::Insert => SegKind::Added,
        };
        // `value()` keeps the trailing `\n` except for a final line that lacks one
        // (`missing_newline()`), which is exactly jsdiff's `b8d` line shape.
        let line = ch.value().to_string();
        match segs.last_mut() {
            Some(s) if s.kind == kind => s.lines.push(line),
            _ => segs.push(Seg {
                kind,
                lines: vec![line],
            }),
        }
    }
    // jsdiff appends a sentinel empty unchanged segment so the final hunk closes.
    segs.push(Seg {
        kind: SegKind::Unchanged,
        lines: Vec::new(),
    });

    let n = segs.len() as i64;
    let mut hunks: Vec<StructuredPatchHunk> = Vec::new();
    let mut old_line: i64 = 1; // jsdiff `g`
    let mut new_line: i64 = 1; // jsdiff `_`
    let mut old_start: i64 = 0; // jsdiff `m` (0 = no open hunk)
    let mut new_start: i64 = 0; // jsdiff `f`
    let mut lines: Vec<String> = Vec::new(); // jsdiff `h`

    for (t, seg) in segs.iter().enumerate() {
        let s_len = seg.lines.len() as i64;
        match seg.kind {
            SegKind::Added | SegKind::Removed => {
                if old_start == 0 {
                    old_start = old_line;
                    new_start = new_line;
                    if t > 0 {
                        // Leading context = the last `PATCH_CONTEXT` lines of the
                        // preceding (unchanged) segment.
                        let prev = &segs[t - 1].lines;
                        let take = prev.len().min(patch_context as usize);
                        for l in &prev[prev.len() - take..] {
                            lines.push(format!(" {l}"));
                        }
                        old_start -= take as i64;
                        new_start -= take as i64;
                    }
                }
                let prefix = if seg.kind == SegKind::Added { '+' } else { '-' };
                for l in &seg.lines {
                    lines.push(format!("{prefix}{l}"));
                }
                if seg.kind == SegKind::Added {
                    new_line += s_len;
                } else {
                    old_line += s_len;
                }
            }
            SegKind::Unchanged => {
                if old_start != 0 {
                    if s_len <= patch_context * 2 && (t as i64) < n - 2 {
                        // Small gap → keep the hunk open, fold it in as context.
                        for l in &seg.lines {
                            lines.push(format!(" {l}"));
                        }
                    } else {
                        // Close the hunk with `min(gap, context)` trailing context.
                        let e = s_len.min(patch_context);
                        for l in seg.lines.iter().take(e as usize) {
                            lines.push(format!(" {l}"));
                        }
                        hunks.push(StructuredPatchHunk {
                            old_start,
                            old_lines: old_line - old_start + e,
                            new_start,
                            new_lines: new_line - new_start + e,
                            lines: std::mem::take(&mut lines),
                        });
                        old_start = 0;
                        new_start = 0;
                    }
                }
                old_line += s_len;
                new_line += s_len;
            }
        }
    }

    // Final pass (jsdiff): strip each line's trailing `\n`, or — when a line lacks
    // one (a no-newline final line) — insert the `\ No newline at end of file`
    // marker immediately after it.
    for hunk in &mut hunks {
        let mut y = 0;
        while y < hunk.lines.len() {
            if hunk.lines[y].ends_with('\n') {
                hunk.lines[y].pop();
            } else {
                hunk.lines
                    .insert(y + 1, "\\ No newline at end of file".to_string());
                y += 1;
            }
            y += 1;
        }
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_replace_one_hunk_with_context() {
        // before/after share line1 + line3; line2 → CHANGED.
        let h = build_structured_patch("line1\nline2\nline3\n", "line1\nCHANGED\nline3\n");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].old_start, 1);
        assert_eq!(h[0].old_lines, 3);
        assert_eq!(h[0].new_start, 1);
        assert_eq!(h[0].new_lines, 3);
        assert_eq!(h[0].lines, vec![" line1", "-line2", "+CHANGED", " line3"]);
    }

    #[test]
    fn pure_insertion_no_old_lines() {
        // Append a line at EOF; the context is the prior lines (≤4).
        let h = build_structured_patch("a\nb\n", "a\nb\nc\n");
        assert_eq!(h.len(), 1);
        // context before = [a, b]; oldStart = 1, oldLines = 2 (just the context).
        assert_eq!(h[0].old_start, 1);
        assert_eq!(h[0].old_lines, 2);
        assert_eq!(h[0].new_start, 1);
        assert_eq!(h[0].new_lines, 3);
        assert_eq!(h[0].lines, vec![" a", " b", "+c"]);
    }

    #[test]
    fn no_change_yields_no_hunks() {
        assert!(build_structured_patch("x\ny\n", "x\ny\n").is_empty());
    }

    #[test]
    fn no_trailing_newline_emits_marker() {
        // The replaced last line lacks a trailing newline → the marker is inserted.
        let h = build_structured_patch("a\nb", "a\nB");
        assert_eq!(h.len(), 1);
        assert_eq!(
            h[0].lines,
            vec![
                " a",
                "-b",
                "\\ No newline at end of file",
                "+B",
                "\\ No newline at end of file"
            ]
        );
    }

    #[test]
    fn serializes_to_camelcase_keys() {
        let h = build_structured_patch("a\nb\nc\n", "a\nX\nc\n");
        let v = serde_json::to_value(&h).unwrap();
        let hunk = &v[0];
        assert!(hunk.get("oldStart").is_some());
        assert!(hunk.get("oldLines").is_some());
        assert!(hunk.get("newStart").is_some());
        assert!(hunk.get("newLines").is_some());
        assert!(hunk.get("lines").is_some());
    }
}
