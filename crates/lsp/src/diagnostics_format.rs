//! Byte-faithful port of claude-code's `Uq` diagnostics formatter
//! (binary v2.1.185 @199005171): `formatDiagnosticsBlock` /
//! `formatDiagnosticsSummary` / `getSeveritySymbol`.
//!
//! This is the passive `<new-diagnostics>` reminder the model receives after a
//! tool/edit cycle introduces new LSP diagnostics. The block text is produced
//! here; the per-uri "already-sent" dedup that decides *which* diagnostics are
//! new lives on [`crate::diagnostic_registry::LspDiagnosticRegistry`].

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

/// `kra` — the summary character cap before truncation (binary @199005xxx).
const MAX_SUMMARY_CHARS: usize = 4000;

/// `getSeveritySymbol(severity)` (binary @199005171): `{Error:cross, Warning,
/// Info, Hint:star}[severity] || bullet`, using the MAIN `figures` glyph set
/// (NOT the ASCII fallback — the block is injected into model-facing text, not
/// rendered to a terminal). Error=`✘`(U+2718), Warning=`⚠`(U+26A0),
/// Info=`ℹ`(U+2139), Hint=`★`(U+2605), anything else=`●`(U+25CF).
#[must_use]
fn severity_symbol(severity: Option<DiagnosticSeverity>) -> char {
    match severity {
        Some(DiagnosticSeverity::ERROR) => '\u{2718}',
        Some(DiagnosticSeverity::WARNING) => '\u{26A0}',
        Some(DiagnosticSeverity::INFORMATION) => '\u{2139}',
        Some(DiagnosticSeverity::HINT) => '\u{2605}',
        _ => '\u{25CF}',
    }
}

/// `code` rendered as the binary's `${i.code}` would: a number prints its
/// digits, a string prints verbatim.
fn code_to_string(code: &NumberOrString) -> String {
    match code {
        NumberOrString::Number(n) => n.to_string(),
        NumberOrString::String(s) => s.clone(),
    }
}

/// One file's diagnostics, in the `{uri, diagnostics}` shape
/// `formatDiagnosticsSummary` consumes.
#[derive(Debug, Clone)]
pub struct DiagnosticFile {
    /// Document URI (the full `file://…` form; the summary shows its basename).
    pub uri: String,
    /// The NEW diagnostics for this file (post-dedup), in server order.
    pub diagnostics: Vec<Diagnostic>,
}

/// `formatDiagnosticsSummary(files)` (binary @199005171).
///
/// Per file: basename (`uri.split('/').pop() || uri`) then a `:` line, then one
/// indented line per diagnostic:
/// `  {symbol} [Line {line+1}:{char+1}] {message}{ [code]}{ (source)}`.
/// Diagnostic lines join with `\n`; files join with `\n\n`. If the result
/// exceeds [`MAX_SUMMARY_CHARS`] it is truncated to `cap-12` chars plus the
/// `…[truncated]` marker (12 chars → total `cap`).
#[must_use]
pub fn format_diagnostics_summary(files: &[DiagnosticFile]) -> String {
    let summary = files
        .iter()
        .map(|file| {
            // `uri.split("/").pop() || uri`: last "/"-segment; a trailing slash
            // yields "" → fall back to the whole uri.
            let basename = file
                .uri
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&file.uri);
            let lines = file
                .diagnostics
                .iter()
                .map(format_one_diagnostic)
                .collect::<Vec<_>>()
                .join("\n");
            format!("{basename}:\n{lines}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    // JS `.length` is UTF-16 code units; `.chars().count()` matches for the BMP
    // glyphs/content here and avoids slicing on a byte boundary.
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        let kept: String = summary.chars().take(MAX_SUMMARY_CHARS - 12).collect();
        format!("{kept}\u{2026}[truncated]")
    } else {
        summary
    }
}

/// One diagnostic line:
/// `  {symbol} [Line {line+1}:{char+1}] {message}{ [code]}{ (source)}`.
fn format_one_diagnostic(d: &Diagnostic) -> String {
    let symbol = severity_symbol(d.severity);
    let line = d.range.start.line + 1;
    let character = d.range.start.character + 1;
    let code = d
        .code
        .as_ref()
        .map(|c| format!(" [{}]", code_to_string(c)))
        .unwrap_or_default();
    let source = d
        .source
        .as_ref()
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    format!(
        "  {symbol} [Line {line}:{character}] {message}{code}{source}",
        message = d.message
    )
}

/// `formatDiagnosticsBlock(files)` (binary @199005171): wrap the summary in the
/// `<new-diagnostics>` envelope.
#[must_use]
pub fn format_diagnostics_block(files: &[DiagnosticFile]) -> String {
    format!(
        "<new-diagnostics>The following new diagnostic issues were detected:\n\n{}</new-diagnostics>",
        format_diagnostics_summary(files)
    )
}

/// `Lra(diagnostic)` (binary @199005171): the per-diagnostic dedup key —
/// `JSON.stringify({message, severity, range, source||null, code||null})`.
/// Only used internally to decide which diagnostics are NEW, so the exact
/// serialization need only be stable, not byte-identical to the binary's JSON.
#[must_use]
pub fn dedup_key(d: &Diagnostic) -> String {
    let severity = match d.severity {
        Some(DiagnosticSeverity::ERROR) => 1,
        Some(DiagnosticSeverity::WARNING) => 2,
        Some(DiagnosticSeverity::INFORMATION) => 3,
        Some(DiagnosticSeverity::HINT) => 4,
        _ => 0,
    };
    let source = d.source.as_deref().unwrap_or("");
    let code = d.code.as_ref().map(code_to_string).unwrap_or_default();
    format!(
        "{severity}\u{1f}{sl}:{sc}-{el}:{ec}\u{1f}{source}\u{1f}{code}\u{1f}{message}",
        sl = d.range.start.line,
        sc = d.range.start.character,
        el = d.range.end.line,
        ec = d.range.end.character,
        message = d.message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diag(line: u32, ch: u32, sev: DiagnosticSeverity, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, ch), Position::new(line, ch + 1)),
            severity: Some(sev),
            message: msg.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn block_envelope_and_summary_byte_exact() {
        let mut d = diag(9, 4, DiagnosticSeverity::ERROR, "cannot find name 'x'");
        d.code = Some(NumberOrString::Number(2304));
        d.source = Some("ts".to_string());
        let files = vec![DiagnosticFile {
            uri: "file:///repo/src/main.ts".to_string(),
            diagnostics: vec![d],
        }];
        let block = format_diagnostics_block(&files);
        assert_eq!(
            block,
            "<new-diagnostics>The following new diagnostic issues were detected:\n\n\
             main.ts:\n  \u{2718} [Line 10:5] cannot find name 'x' [2304] (ts)</new-diagnostics>"
        );
    }

    #[test]
    fn severity_glyphs_and_optional_fields() {
        let files = vec![DiagnosticFile {
            uri: "file:///a/b.rs".to_string(),
            diagnostics: vec![
                diag(0, 0, DiagnosticSeverity::WARNING, "unused"),
                diag(1, 2, DiagnosticSeverity::INFORMATION, "info"),
                diag(2, 0, DiagnosticSeverity::HINT, "hint"),
            ],
        }];
        let s = format_diagnostics_summary(&files);
        assert_eq!(
            s,
            "b.rs:\n  \u{26a0} [Line 1:1] unused\n  \u{2139} [Line 2:3] info\n  \u{2605} [Line 3:1] hint"
        );
    }

    #[test]
    fn multiple_files_join_with_blank_line() {
        let files = vec![
            DiagnosticFile {
                uri: "file:///x.ts".to_string(),
                diagnostics: vec![diag(0, 0, DiagnosticSeverity::ERROR, "e1")],
            },
            DiagnosticFile {
                uri: "file:///y.ts".to_string(),
                diagnostics: vec![diag(0, 0, DiagnosticSeverity::ERROR, "e2")],
            },
        ];
        let s = format_diagnostics_summary(&files);
        assert_eq!(
            s,
            "x.ts:\n  \u{2718} [Line 1:1] e1\n\ny.ts:\n  \u{2718} [Line 1:1] e2"
        );
    }

    #[test]
    fn trailing_slash_uri_falls_back_to_whole_uri() {
        let files = vec![DiagnosticFile {
            uri: "file:///dir/".to_string(),
            diagnostics: vec![diag(0, 0, DiagnosticSeverity::ERROR, "e")],
        }];
        let s = format_diagnostics_summary(&files);
        assert!(s.starts_with("file:///dir/:\n"), "got: {s}");
    }

    #[test]
    fn summary_truncates_at_cap() {
        // Build enough diagnostics to exceed 4000 chars.
        let many: Vec<Diagnostic> = (0..400)
            .map(|i| {
                diag(
                    i,
                    0,
                    DiagnosticSeverity::ERROR,
                    "a fairly long diagnostic message here",
                )
            })
            .collect();
        let files = vec![DiagnosticFile {
            uri: "file:///big.ts".to_string(),
            diagnostics: many,
        }];
        let s = format_diagnostics_summary(&files);
        assert_eq!(s.chars().count(), MAX_SUMMARY_CHARS);
        assert!(s.ends_with("\u{2026}[truncated]"));
    }

    #[test]
    fn dedup_key_distinguishes_message_and_range() {
        let a = diag(0, 0, DiagnosticSeverity::ERROR, "x");
        let b = diag(0, 0, DiagnosticSeverity::ERROR, "y");
        let c = diag(1, 0, DiagnosticSeverity::ERROR, "x");
        assert_ne!(dedup_key(&a), dedup_key(&b));
        assert_ne!(dedup_key(&a), dedup_key(&c));
        assert_eq!(
            dedup_key(&a),
            dedup_key(&diag(0, 0, DiagnosticSeverity::ERROR, "x"))
        );
    }
}
