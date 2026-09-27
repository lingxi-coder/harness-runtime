//! FileRead PDF page-image extraction: spawn `pdftoppm` (poppler-utils) to render
//! pages to JPEG, mirroring claude-code `src/utils/pdf.ts` `extractPDFPages`.
//! `pdftoppm` is a RUNTIME system dependency (no Rust crate). The pure helpers
//! (`build_pdftoppm_args`, `classify_pdftoppm_failure`) carry the logic
//! claude-code tests by mocking the subprocess; the spawn itself
//! (`render_pdf_pages`) is exercised by the availability-gated smoke test.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::pdf_read::PDF_MAX_EXTRACT_SIZE;

/// pdftoppm render timeout (claude-code `execFileNoThrow` `timeout: 120_000`).
const PDFTOPPM_TIMEOUT: Duration = Duration::from_secs(120);

/// pdftoppm availability-probe timeout (claude-code `isPdftoppmAvailable`
/// `execFileNoThrow('pdftoppm', ['-v'], { timeout: 5000 })`).
const PDFTOPPM_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Structured failure of PDF page extraction — mirrors claude-code `PDFError`
/// (`src/utils/pdf.ts`). Each variant maps to a fixed user-facing message and a
/// stable telemetry code.
#[derive(Debug)]
pub enum PdfRenderError {
    /// 0-byte file.
    Empty,
    /// Larger than [`PDF_MAX_EXTRACT_SIZE`] (100 MB).
    TooLarge,
    /// `pdftoppm` (poppler-utils) is not installed / not on `PATH`.
    Unavailable,
    /// pdftoppm reported the PDF is password-protected.
    PasswordProtected,
    /// pdftoppm reported the PDF is damaged/corrupt/invalid.
    Corrupted,
    /// pdftoppm exited 0 but produced no `.jpg` pages.
    NoOutput,
    /// Any other non-zero exit; carries the captured stderr.
    Unknown(String),
}

impl PdfRenderError {
    /// The exact claude-code user-facing message for this failure. `path` is the
    /// PDF path (used only by [`Self::Empty`]).
    #[must_use]
    pub fn message(&self, path: &str) -> String {
        match self {
            Self::Empty => format!("PDF file is empty: {path}"),
            Self::TooLarge => format!(
                "PDF file exceeds maximum allowed size for text extraction ({}).",
                crate::read::format_file_size(PDF_MAX_EXTRACT_SIZE)
            ),
            Self::Unavailable => "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering.".to_string(),
            Self::PasswordProtected => "PDF is password-protected. Please provide an unprotected version.".to_string(),
            Self::Corrupted => "PDF file is corrupted or invalid.".to_string(),
            Self::NoOutput => "pdftoppm produced no output pages. The PDF may be invalid.".to_string(),
            Self::Unknown(stderr) => format!("pdftoppm failed: {stderr}"),
        }
    }

    /// Stable telemetry code for `emit_failed`.
    #[must_use]
    pub fn telemetry_code(&self) -> &'static str {
        match self {
            Self::Empty => "pdf_empty",
            Self::TooLarge => "pdf_too_large",
            Self::Unavailable => "pdf_unavailable",
            Self::PasswordProtected => "pdf_password_protected",
            Self::Corrupted => "pdf_corrupted",
            Self::NoOutput => "pdf_no_output",
            Self::Unknown(_) => "pdf_unknown",
        }
    }
}

/// Build the `pdftoppm` argument vector (claude-code `src/utils/pdf.ts:222-230`).
/// `-jpeg -r 100` are always present. Then `-f first` is pushed ONLY when
/// `first != 0` — mirroring claude-code's `if (options?.firstPage)` (pdf.ts:224-226),
/// where `0` is JS-falsy/undefined so the flag is omitted. Then `-l last` UNLESS
/// `last == u32::MAX` (the open-ended `"N-"` range — claude-code's `Infinity` —
/// renders to the end). Finally the input path and the output `<dir>/page` prefix.
/// In practice the pages path always supplies `first >= 1`, so `-f` is present
/// for every real call; the `first == 0` branch exists purely for faithful parity.
#[must_use]
pub fn build_pdftoppm_args(first: u32, last: u32, input: &Path, prefix: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-jpeg".into(), "-r".into(), "100".into()];
    if first != 0 {
        args.push("-f".into());
        args.push(first.to_string().into());
    }
    if last != u32::MAX {
        args.push("-l".into());
        args.push(last.to_string().into());
    }
    args.push(input.as_os_str().to_owned());
    args.push(prefix.as_os_str().to_owned());
    args
}

/// Classify a non-zero `pdftoppm` exit by its stderr (claude-code
/// `src/utils/pdf.ts:236-259`): `/password/i` → password-protected;
/// `/damaged|corrupt|invalid/i` → corrupted; else `Unknown(stderr)`.
#[must_use]
pub fn classify_pdftoppm_failure(stderr: &str) -> PdfRenderError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("password") {
        PdfRenderError::PasswordProtected
    } else if lower.contains("damaged") || lower.contains("corrupt") || lower.contains("invalid") {
        PdfRenderError::Corrupted
    } else {
        PdfRenderError::Unknown(stderr.to_string())
    }
}

/// Whether `pdftoppm -v` runs (poppler-utils installed). claude-code
/// (`isPdftoppmAvailable`, pdf.ts:160-169) treats either exit 0 OR any stderr
/// output as "available" (pdftoppm prints its version banner to stderr and may
/// exit non-zero on old builds). Bounded by [`PDFTOPPM_PROBE_TIMEOUT`] (5s, like
/// claude-code's `execFileNoThrow(..., { timeout: 5000 })`); a timeout, spawn
/// error, or not-found all map to "not available" (`false`). `kill_on_drop`
/// ensures a hung probe is reaped on timeout. Not cached — extraction is rare,
/// so we probe per call (claude-code caches purely as a perf optimization;
/// behavior is identical).
pub async fn is_pdftoppm_available() -> bool {
    let child = tokio::process::Command::new("pdftoppm")
        .arg("-v")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let Ok(child) = child else {
        return false; // not found on PATH / spawn error
    };
    match tokio::time::timeout(PDFTOPPM_PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out.status.success() || !out.stderr.is_empty(),
        // timeout (child dropped → SIGKILL) or wait error ⇒ treat as unavailable.
        _ => false,
    }
}

/// Render pages `first..=last` (1-indexed; `last == u32::MAX` ⇒ to end) of the
/// PDF at `path` to JPEG via `pdftoppm`, returning the page images in natural
/// filename order. Ports claude-code `extractPDFPages` (pdf.ts:179-300).
///
/// `original_size` is the already-stat'd file size (the caller has it) and is
/// rejected if `> PDF_MAX_EXTRACT_SIZE`.
///
/// # Errors
/// [`PdfRenderError`] for too-large input, missing `pdftoppm`, a password /
/// corrupted PDF, no output pages, a render timeout, or any I/O failure.
pub async fn render_pdf_pages(
    path: &Path,
    original_size: u64,
    first: u32,
    last: u32,
) -> Result<Vec<Vec<u8>>, PdfRenderError> {
    if original_size == 0 {
        return Err(PdfRenderError::Empty);
    }
    if original_size > PDF_MAX_EXTRACT_SIZE {
        return Err(PdfRenderError::TooLarge);
    }
    if !is_pdftoppm_available().await {
        return Err(PdfRenderError::Unavailable);
    }

    // Scratch dir (auto-removed on drop). pdftoppm writes <dir>/page-NN.jpg.
    let dir = tempfile::Builder::new()
        .prefix("lingxi-pdf-")
        .tempdir()
        .map_err(|e| PdfRenderError::Unknown(e.to_string()))?;
    let prefix = dir.path().join("page");
    let args = build_pdftoppm_args(first, last, path, &prefix);

    // Spawn (not `.output()`) so the timeout future owns the `Child`: with
    // `kill_on_drop(true)`, dropping it on timeout SIGKILLs pdftoppm instead of
    // orphaning it (tokio `Child` defaults to `kill_on_drop = false`).
    let child = tokio::process::Command::new("pdftoppm")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| PdfRenderError::Unknown(e.to_string()))?;

    let output = tokio::time::timeout(PDFTOPPM_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| PdfRenderError::Unknown("pdftoppm timed out".to_string()))?
        .map_err(|e| PdfRenderError::Unknown(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(classify_pdftoppm_failure(&stderr));
    }

    // Collect *.jpg, sort by filename (claude-code `entries.filter(...).sort()`).
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir.path())
        .map_err(|e| PdfRenderError::Unknown(e.to_string()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jpg"))
        .collect();
    files.sort();

    if files.is_empty() {
        return Err(PdfRenderError::NoOutput);
    }

    let mut pages = Vec::with_capacity(files.len());
    for f in files {
        pages.push(std::fs::read(&f).map_err(|e| PdfRenderError::Unknown(e.to_string()))?);
    }
    Ok(pages)
    // `dir` drops here → scratch removed.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn args_as_strings(a: &[std::ffi::OsString]) -> Vec<String> {
        a.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn args_single_page_pushes_f_and_l() {
        let a = build_pdftoppm_args(5, 5, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec![
                "-jpeg",
                "-r",
                "100",
                "-f",
                "5",
                "-l",
                "5",
                "/in.pdf",
                "/tmp/x/page"
            ]
        );
    }

    #[test]
    fn args_range_pushes_f_and_l() {
        let a = build_pdftoppm_args(2, 7, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec![
                "-jpeg",
                "-r",
                "100",
                "-f",
                "2",
                "-l",
                "7",
                "/in.pdf",
                "/tmp/x/page"
            ]
        );
    }

    #[test]
    fn args_first_zero_omits_f() {
        // first == 0 models claude-code's JS-falsy `firstPage` (pdf.ts:224-226):
        // the `if (options?.firstPage)` guard is false, so -f is NOT pushed.
        let a = build_pdftoppm_args(0, 5, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec!["-jpeg", "-r", "100", "-l", "5", "/in.pdf", "/tmp/x/page"]
        );
    }

    #[test]
    fn args_open_ended_omits_l() {
        // u32::MAX models the open-ended "N-" range (claude-code Infinity): -l is
        // NOT pushed, so pdftoppm renders from firstPage to the end of the doc.
        let a = build_pdftoppm_args(3, u32::MAX, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec!["-jpeg", "-r", "100", "-f", "3", "/in.pdf", "/tmp/x/page"]
        );
    }

    #[test]
    fn classify_password() {
        assert!(matches!(
            classify_pdftoppm_failure("Error: PDF file is password protected"),
            PdfRenderError::PasswordProtected
        ));
        // case-insensitive (claude-code /password/i)
        assert!(matches!(
            classify_pdftoppm_failure("Command Line Error: Incorrect PASSWORD"),
            PdfRenderError::PasswordProtected
        ));
    }

    #[test]
    fn classify_corrupted() {
        for s in [
            "the file is damaged",
            "May not be a PDF (corrupt)",
            "Invalid XRef",
        ] {
            assert!(
                matches!(classify_pdftoppm_failure(s), PdfRenderError::Corrupted),
                "stderr {s:?} should classify corrupted"
            );
        }
    }

    #[test]
    fn classify_unknown_carries_stderr() {
        match classify_pdftoppm_failure("some other failure") {
            PdfRenderError::Unknown(s) => assert_eq!(s, "some other failure"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn error_messages_match_claude_code() {
        assert_eq!(
            PdfRenderError::Empty.message("/a.pdf"),
            "PDF file is empty: /a.pdf"
        );
        assert_eq!(
            PdfRenderError::TooLarge.message("/a.pdf"),
            "PDF file exceeds maximum allowed size for text extraction (100MB)."
        );
        assert_eq!(
            PdfRenderError::Unavailable.message("/a.pdf"),
            "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering."
        );
        assert_eq!(
            PdfRenderError::PasswordProtected.message("/a.pdf"),
            "PDF is password-protected. Please provide an unprotected version."
        );
        assert_eq!(
            PdfRenderError::Corrupted.message("/a.pdf"),
            "PDF file is corrupted or invalid."
        );
        assert_eq!(
            PdfRenderError::NoOutput.message("/a.pdf"),
            "pdftoppm produced no output pages. The PDF may be invalid."
        );
        assert_eq!(
            PdfRenderError::Unknown("boom".to_string()).message("/a.pdf"),
            "pdftoppm failed: boom"
        );
    }

    #[test]
    fn telemetry_codes_are_stable() {
        assert_eq!(PdfRenderError::Empty.telemetry_code(), "pdf_empty");
        assert_eq!(PdfRenderError::TooLarge.telemetry_code(), "pdf_too_large");
        assert_eq!(
            PdfRenderError::Unavailable.telemetry_code(),
            "pdf_unavailable"
        );
        assert_eq!(
            PdfRenderError::PasswordProtected.telemetry_code(),
            "pdf_password_protected"
        );
        assert_eq!(PdfRenderError::Corrupted.telemetry_code(), "pdf_corrupted");
        assert_eq!(PdfRenderError::NoOutput.telemetry_code(), "pdf_no_output");
        assert_eq!(
            PdfRenderError::Unknown(String::new()).telemetry_code(),
            "pdf_unknown"
        );
    }

    // A valid minimal 1-page PDF (same fixture pdf_read uses). pdftoppm renders one
    // blank page from it.
    const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\nxref\n0 4\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n186\n%%EOF\n";

    #[tokio::test]
    async fn render_pdf_pages_smoke() {
        // ENV-GATED: poppler-utils may be absent (dev machine + some CI). Skip cleanly
        // rather than fail — the parity-critical logic is covered by the pure-helper
        // and build_pages_payload tests; this only exercises the real spawn when it can.
        if !is_pdftoppm_available().await {
            eprintln!("skipping render_pdf_pages_smoke: pdftoppm not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("one.pdf");
        std::fs::write(&pdf, MINIMAL_PDF).unwrap();

        let pages = render_pdf_pages(&pdf, MINIMAL_PDF.len() as u64, 1, 1)
            .await
            .expect("render a 1-page PDF");
        assert_eq!(pages.len(), 1, "one JPEG for a 1-page PDF");
        assert!(pages[0].starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
    }

    #[tokio::test]
    async fn render_rejects_oversize() {
        // No pdftoppm needed: the size guard short-circuits before the spawn.
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("big.pdf");
        std::fs::write(&pdf, MINIMAL_PDF).unwrap();
        let err = render_pdf_pages(&pdf, PDF_MAX_EXTRACT_SIZE + 1, 1, 1)
            .await
            .unwrap_err();
        assert!(matches!(err, PdfRenderError::TooLarge));
    }
}
