//! Source-preserving formatter for the passive `<new-diagnostics>` reminder.
//!
//! Diagnostic text arrives from an LSP server and may contain arbitrary
//! JavaScript UTF-16 strings. Keep those fields in `Utf16JsonProjection`
//! through formatting so an unpaired surrogate survives to the model.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lsp_types::{DiagnosticSeverity, Range};

/// `kra` — the summary character cap before truncation (binary @199005xxx).
const MAX_SUMMARY_CHARS: usize = 4000;

/// A diagnostic code whose string alternative remains exact UTF-16 source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticCodeSource {
    /// Numeric LSP diagnostic code.
    Number(i32),
    /// String LSP diagnostic code, retained as an exact root string.
    String(Utf16JsonProjection),
}

/// Diagnostic fields used by the passive model-facing formatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDiagnostic {
    /// Source range, represented by the protocol's typed integer positions.
    pub range: Range,
    /// Optional severity from the LSP notification.
    pub severity: Option<DiagnosticSeverity>,
    /// Required diagnostic message, retained as exact UTF-16 source.
    pub message: Utf16JsonProjection,
    /// Optional diagnostic source label, retained as exact UTF-16 source.
    pub source: Option<Utf16JsonProjection>,
    /// Optional numeric or exact string diagnostic code.
    pub code: Option<DiagnosticCodeSource>,
}

impl SourceDiagnostic {
    /// Build a generated diagnostic with no optional source or code fields.
    #[must_use]
    pub fn new(
        range: Range,
        severity: Option<DiagnosticSeverity>,
        message: Utf16JsonProjection,
    ) -> Self {
        Self {
            range,
            severity,
            message,
            source: None,
            code: None,
        }
    }
}

/// One file's diagnostics, in the `{uri, diagnostics}` shape consumed by the
/// TypeScript-compatible summary formatter. The URI has crossed `Url::parse`
/// at the protocol boundary and is therefore valid Rust text.
#[derive(Debug, Clone)]
pub struct DiagnosticFile {
    /// Document URI; the summary shows its final slash-separated component.
    pub uri: String,
    /// New diagnostics for this file, in server order.
    pub diagnostics: Vec<SourceDiagnostic>,
}

/// `getSeveritySymbol(severity)` (binary @199005171): `{Error:cross, Warning,
/// Info, Hint:star}[severity] || bullet`, using the main figure glyph set.
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

fn append_text(output: &mut Vec<u16>, text: &str) {
    output.extend(text.encode_utf16());
}

fn append_exact_string(output: &mut Vec<u16>, value: &Utf16JsonProjection) {
    if let Some(units) = value.string_units("") {
        output.extend(units);
    }
}

fn diagnostic_code_json(code: &DiagnosticCodeSource) -> String {
    match code {
        DiagnosticCodeSource::Number(number) => number.to_string(),
        DiagnosticCodeSource::String(value) => value
            .to_json_string()
            .expect("validated diagnostic code projection stringifies"),
    }
}

fn format_one_diagnostic(diagnostic: &SourceDiagnostic, output: &mut Vec<u16>) {
    let symbol = severity_symbol(diagnostic.severity);
    let line = diagnostic.range.start.line + 1;
    let character = diagnostic.range.start.character + 1;

    append_text(output, "  ");
    append_text(output, &symbol.to_string());
    append_text(output, &format!(" [Line {line}:{character}] "));
    append_exact_string(output, &diagnostic.message);
    if let Some(code) = &diagnostic.code {
        append_text(output, " [");
        match code {
            DiagnosticCodeSource::Number(number) => append_text(output, &number.to_string()),
            DiagnosticCodeSource::String(value) => append_exact_string(output, value),
        }
        append_text(output, "]");
    }
    if let Some(source) = &diagnostic.source {
        append_text(output, " (");
        append_exact_string(output, source);
        append_text(output, ")");
    }
}

fn diagnostics_summary_units(files: &[DiagnosticFile]) -> Vec<u16> {
    let mut output = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        if file_index > 0 {
            append_text(&mut output, "\n\n");
        }
        let basename = file
            .uri
            .rsplit('/')
            .next()
            .filter(|part| !part.is_empty())
            .unwrap_or(&file.uri);
        append_text(&mut output, basename);
        append_text(&mut output, ":\n");
        for (diagnostic_index, diagnostic) in file.diagnostics.iter().enumerate() {
            if diagnostic_index > 0 {
                append_text(&mut output, "\n");
            }
            format_one_diagnostic(diagnostic, &mut output);
        }
    }

    // JavaScript String.length and slice count UTF-16 code units. Keep this
    // truncation at the same boundary, including a possible split surrogate.
    if output.len() > MAX_SUMMARY_CHARS {
        output.truncate(MAX_SUMMARY_CHARS - 12);
        append_text(&mut output, "\u{2026}[truncated]");
    }
    output
}

fn projection_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display form is derived from the exact UTF-16 units")
}

/// Format a diagnostic summary as an exact root string.
#[must_use]
pub fn format_diagnostics_summary(files: &[DiagnosticFile]) -> Utf16JsonProjection {
    projection_from_units(diagnostics_summary_units(files))
}

/// `formatDiagnosticsBlock(files)` (binary @199005171), preserving diagnostic
/// strings in exact UTF-16 units.
#[must_use]
pub fn format_diagnostics_block(files: &[DiagnosticFile]) -> Utf16JsonProjection {
    let mut output = Vec::new();
    append_text(
        &mut output,
        "<new-diagnostics>The following new diagnostic issues were detected:\n\n",
    );
    output.extend(diagnostics_summary_units(files));
    append_text(&mut output, "</new-diagnostics>");
    projection_from_units(output)
}

/// Stable exact deduplication key for the model-visible diagnostic fields.
/// JSON stringification escapes unpaired UTF-16 units, so the resulting key is
/// valid Rust text without replacing the source value.
#[must_use]
pub fn dedup_key(diagnostic: &SourceDiagnostic) -> String {
    let severity = match diagnostic.severity {
        Some(DiagnosticSeverity::ERROR) => 1,
        Some(DiagnosticSeverity::WARNING) => 2,
        Some(DiagnosticSeverity::INFORMATION) => 3,
        Some(DiagnosticSeverity::HINT) => 4,
        _ => 0,
    };
    let source = diagnostic
        .source
        .as_ref()
        .filter(|value| {
            value
                .string_units("")
                .is_some_and(|units| !units.is_empty())
        })
        .map(|value| value.to_json_string().expect("validated source string"))
        .unwrap_or_else(|| "null".to_string());
    let code = diagnostic
        .code
        .as_ref()
        .map(diagnostic_code_json)
        .unwrap_or_else(|| "null".to_string());
    let message = diagnostic
        .message
        .to_json_string()
        .expect("validated diagnostic message stringifies");
    format!(
        "{severity}\u{1f}{sl}:{sc}-{el}:{ec}\u{1f}{source}\u{1f}{code}\u{1f}{message}",
        sl = diagnostic.range.start.line,
        sc = diagnostic.range.start.character,
        el = diagnostic.range.end.line,
        ec = diagnostic.range.end.character,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn source_string(units: Vec<u16>) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).unwrap()
    }

    fn diag(line: u32, ch: u32, sev: DiagnosticSeverity, msg: &str) -> SourceDiagnostic {
        SourceDiagnostic::new(
            Range::new(Position::new(line, ch), Position::new(line, ch + 1)),
            Some(sev),
            msg.into(),
        )
    }

    #[test]
    fn block_envelope_and_summary_byte_exact() {
        let mut d = diag(9, 4, DiagnosticSeverity::ERROR, "cannot find name 'x'");
        d.code = Some(DiagnosticCodeSource::Number(2304));
        d.source = Some("ts".into());
        let files = vec![DiagnosticFile {
            uri: "file:///repo/src/main.ts".to_string(),
            diagnostics: vec![d],
        }];
        let block = format_diagnostics_block(&files);
        assert_eq!(
            block.string_at(""),
            Some(
                "<new-diagnostics>The following new diagnostic issues were detected:\n\n\
             main.ts:\n  \u{2718} [Line 10:5] cannot find name 'x' [2304] (ts)</new-diagnostics>"
            )
        );
    }

    #[test]
    fn exact_utf16_message_source_and_code_reach_model_block() {
        let mut diagnostic = diag(0, 0, DiagnosticSeverity::ERROR, "safe");
        diagnostic.message = source_string(vec![b'x' as u16, 0xd800]);
        diagnostic.source = Some(source_string(vec![b'l' as u16, 0xdc00]));
        diagnostic.code = Some(DiagnosticCodeSource::String(source_string(vec![0xdfff])));
        let block = format_diagnostics_block(&[DiagnosticFile {
            uri: "file:///workspace/a.ts".into(),
            diagnostics: vec![diagnostic],
        }]);
        assert_eq!(
            block.string_units(""),
            Some(
                "<new-diagnostics>The following new diagnostic issues were detected:\n\n\
                 a.ts:\n  \u{2718} [Line 1:1] x"
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(" [".encode_utf16())
                    .chain([0xdfff])
                    .chain("] (l".encode_utf16())
                    .chain([0xdc00])
                    .chain(")</new-diagnostics>".encode_utf16())
                    .collect::<Vec<_>>(),
            )
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
        assert_eq!(
            format_diagnostics_summary(&files).string_at(""),
            Some("b.rs:\n  \u{26a0} [Line 1:1] unused\n  \u{2139} [Line 2:3] info\n  \u{2605} [Line 3:1] hint")
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
        assert_eq!(
            format_diagnostics_summary(&files).string_at(""),
            Some("x.ts:\n  \u{2718} [Line 1:1] e1\n\ny.ts:\n  \u{2718} [Line 1:1] e2")
        );
    }

    #[test]
    fn trailing_slash_uri_falls_back_to_whole_uri() {
        let files = vec![DiagnosticFile {
            uri: "file:///dir/".to_string(),
            diagnostics: vec![diag(0, 0, DiagnosticSeverity::ERROR, "e")],
        }];
        assert!(format_diagnostics_summary(&files)
            .string_at("")
            .unwrap()
            .starts_with("file:///dir/:\n"));
    }

    #[test]
    fn summary_truncates_at_utf16_unit_cap() {
        let many: Vec<SourceDiagnostic> = (0..400)
            .map(|i| {
                diag(
                    i,
                    0,
                    DiagnosticSeverity::ERROR,
                    "a fairly long diagnostic message here",
                )
            })
            .collect();
        let summary = format_diagnostics_summary(&[DiagnosticFile {
            uri: "file:///big.ts".to_string(),
            diagnostics: many,
        }]);
        assert_eq!(summary.string_units("").unwrap().len(), MAX_SUMMARY_CHARS);
        assert!(summary
            .string_at("")
            .unwrap()
            .ends_with("\u{2026}[truncated]"));
    }

    #[test]
    fn dedup_key_distinguishes_exact_message_and_range() {
        let a = diag(0, 0, DiagnosticSeverity::ERROR, "x");
        let b = diag(0, 0, DiagnosticSeverity::ERROR, "y");
        let c = diag(1, 0, DiagnosticSeverity::ERROR, "x");
        assert_ne!(dedup_key(&a), dedup_key(&b));
        assert_ne!(dedup_key(&a), dedup_key(&c));
        assert_eq!(
            dedup_key(&a),
            dedup_key(&diag(0, 0, DiagnosticSeverity::ERROR, "x"))
        );
        assert_ne!(
            dedup_key(&SourceDiagnostic {
                message: source_string(vec![0xd800]),
                ..a.clone()
            }),
            dedup_key(&SourceDiagnostic {
                message: source_string(vec![0xd801]),
                ..a
            })
        );
    }
}
