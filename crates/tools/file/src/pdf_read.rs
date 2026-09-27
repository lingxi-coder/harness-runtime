//! FileRead PDF reading: detect, page-count (via lopdf), and the routing helpers
//! that mirror claude-code FileRead's PDF branch (utils/pdf.ts + pdfUtils.ts).
//! lopdf is a pure-Rust PARSER (page count) — page-image extraction is P4b.

/// Max pages per ranged request.
pub const PDF_MAX_PAGES_PER_READ: u32 = 20;
/// Max page count for an inline (no-range) read.
pub const PDF_AT_MENTION_INLINE_THRESHOLD: u32 = 10;
/// Max raw size for an INLINE document read — claude-code `PDF_TARGET_RAW_SIZE`
/// (apiLimits.ts, 20 MB). Above this, `readPDF` returns `too_large`.
pub const PDF_TARGET_RAW_SIZE: u64 = 20 * 1024 * 1024;
/// Max raw size for PAGE EXTRACTION — claude-code `PDF_MAX_EXTRACT_SIZE`
/// (apiLimits.ts, 100 MB). Above this, `extractPDFPages` returns `too_large`.
pub const PDF_MAX_EXTRACT_SIZE: u64 = 100 * 1024 * 1024;

/// `.pdf` extension (claude-code DOCUMENT_EXTENSIONS).
#[must_use]
pub fn is_pdf_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        == Some("pdf")
}

/// `%PDF-` magic (claude-code readPDF header check).
#[must_use]
pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

/// claude-code isPDFSupported: every model EXCEPT one whose id contains
/// `claude-3-haiku`.
#[must_use]
pub fn is_pdf_supported(model: &str) -> bool {
    !model.to_ascii_lowercase().contains("claude-3-haiku")
}

/// 1-indexed inclusive page range (claude-code parsePDFPageRange). `lastPage` is
/// `u32::MAX` for open-ended `"N-"`. `None` on invalid/empty/inverted.
#[must_use]
pub fn parse_pdf_page_range(pages: &str) -> Option<(u32, u32)> {
    let t = pages.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(left) = t.strip_suffix('-') {
        let first: u32 = left.trim().parse().ok()?;
        return (first >= 1).then_some((first, u32::MAX));
    }
    if let Some((l, r)) = t.split_once('-') {
        let first: u32 = l.trim().parse().ok()?;
        let last: u32 = r.trim().parse().ok()?;
        return (first >= 1 && last >= 1 && last >= first).then_some((first, last));
    }
    let page: u32 = t.parse().ok()?;
    (page >= 1).then_some((page, page))
}

/// Page count via lopdf (claude-code getPDFPageCount). `None` if the bytes don't
/// parse (mirrors pdfinfo returning null → the >10-page gate is skipped).
#[must_use]
pub fn pdf_page_count(bytes: &[u8]) -> Option<u32> {
    let doc = lopdf::Document::load_mem(bytes).ok()?;
    u32::try_from(doc.get_pages().len()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pdf_extension_and_magic() {
        assert!(is_pdf_path(std::path::Path::new("a.pdf")));
        assert!(is_pdf_path(std::path::Path::new("a.PDF")));
        assert!(!is_pdf_path(std::path::Path::new("a.png")));
        assert!(looks_like_pdf(b"%PDF-1.7\n..."));
        assert!(!looks_like_pdf(b"not a pdf"));
    }

    #[test]
    fn parse_page_range_cases() {
        assert_eq!(parse_pdf_page_range("3"), Some((3, 3)));
        assert_eq!(parse_pdf_page_range("1-5"), Some((1, 5)));
        assert_eq!(parse_pdf_page_range("10-20"), Some((10, 20)));
        assert_eq!(parse_pdf_page_range("3-"), Some((3, u32::MAX)));
        assert_eq!(parse_pdf_page_range(""), None);
        assert_eq!(parse_pdf_page_range("0"), None);
        assert_eq!(parse_pdf_page_range("5-1"), None);
        assert_eq!(parse_pdf_page_range("x"), None);
    }

    #[test]
    fn minimal_pdf_page_count_is_one() {
        // A valid minimal 1-page PDF (proper xref table + startxref). lopdf
        // 0.34 rejects PDFs without an xref table, so this fixture carries one;
        // it is the same fixture the read.rs inline-success test uses.
        let bytes: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\nxref\n0 4\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n186\n%%EOF\n";
        assert_eq!(pdf_page_count(bytes), Some(1));
        assert_eq!(pdf_page_count(b"not a pdf"), None);
    }

    #[test]
    fn is_pdf_supported_excludes_haiku3() {
        assert!(is_pdf_supported("claude-sonnet-4-6"));
        assert!(!is_pdf_supported("claude-3-haiku-20240307"));
        assert!(!is_pdf_supported("anthropic.claude-3-haiku"));
    }
}
