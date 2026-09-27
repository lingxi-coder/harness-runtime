//! Binary-content persistence for `WebFetchTool` — faithful port of the helpers
//! in `claude-code/src/tools/WebFetchTool/utils.ts` that write a non-text fetch
//! body to a temp file and append a footer pointing at it.
//!
//! Ported functions (binary offsets are from `claude.exe` v2.1.183):
//! - [`is_binary_content_type`] — `U7r(e)` (offset ~198766178): a content-type is
//!   binary unless it is empty, `text/*`, `+json` / `application/json`, `+xml` /
//!   `application/xml`, `application/javascript*`, or
//!   `application/x-www-form-urlencoded`.
//! - [`mime_extension`] — `iZi(e)` (offset ~198765080): map a mime type to a file
//!   extension (`bin` for unknown/absent).
//! - [`human_size`] — `Ma(e)` (offset ~192345678): human-readable byte size
//!   (`123 bytes` / `1.5KB` / `1MB` / `2.3GB`, with `.0` trimmed).
//! - [`persisted_filename`] — the `webfetch-${Date.now()}-${rand36(6)}` stem used
//!   for the temp file (`B$e`'s `A` argument). The final on-disk name is
//!   `<stem>.<ext>` (`B$e` writes `${A}.${r}`).
//! - [`persist_binary_content`] — `B$e(i,a,A)` (offset ~198766508): write the raw
//!   bytes to `<output_dir>/<stem>.<ext>`, returning the path + size, or an error
//!   string on failure (the caller skips the footer on error, matching
//!   `if(!("error"in h))`).
//! - [`append_binary_footer`] — the `if(A)_+=\`…also saved to ${A}\`` step in the
//!   WebFetch tool `call()`.

use std::path::{Path, PathBuf};

/// `U7r(e)`: is this `Content-Type` a *binary* type that should be persisted?
///
/// Byte-faithful to the binary (`function U7r(e){ … }`): the type before the
/// first `;` is lowercased and trimmed; it is NOT binary when it is empty, starts
/// with `text/`, ends with `+json` or equals `application/json`, ends with `+xml`
/// or equals `application/xml`, starts with `application/javascript`, or equals
/// `application/x-www-form-urlencoded`. Everything else IS binary.
#[must_use]
pub fn is_binary_content_type(content_type: &str) -> bool {
    if content_type.is_empty() {
        return false;
    }
    // `Di(e,";")` — substring before the first `;`, then trim + lowercase.
    let t = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if t.is_empty() {
        return false;
    }
    if t.starts_with("text/") {
        return false;
    }
    if t.ends_with("+json") || t == "application/json" {
        return false;
    }
    if t.ends_with("+xml") || t == "application/xml" {
        return false;
    }
    if t.starts_with("application/javascript") {
        return false;
    }
    if t == "application/x-www-form-urlencoded" {
        return false;
    }
    true
}

/// `iZi(e)`: map a mime type to a file extension (no leading dot). Unknown or
/// absent → `bin`. Byte-faithful to the binary's `switch` (the WebFetch + MCP
/// paths share this function).
#[must_use]
pub fn mime_extension(content_type: &str) -> &'static str {
    let mt = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mt.as_str() {
        "application/pdf" => "pdf",
        "application/json" => "json",
        "text/csv" => "csv",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/markdown" => "md",
        "application/zip" => "zip",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/msword" => "doc",
        "application/vnd.ms-excel" => "xls",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// `Ma(e)`: human-readable byte size. Byte-faithful (incl. the `.0`-trimming):
/// `Ma(900) == "900 bytes"`, `Ma(1536) == "1.5KB"`, `Ma(1048576) == "1MB"`.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let kb = bytes as f64 / 1024.0;
    if kb < 1.0 {
        return format!("{bytes} bytes");
    }
    if kb < 1024.0 {
        return format!("{}KB", trim_dot_zero(kb));
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{}MB", trim_dot_zero(mb));
    }
    let gb = mb / 1024.0;
    format!("{}GB", trim_dot_zero(gb))
}

/// `n.toFixed(1).replace(/\.0$/, '')` — one decimal place, then strip a trailing
/// `.0` (`1.0 -> "1"`, `1.5 -> "1.5"`).
fn trim_dot_zero(n: f64) -> String {
    let s = format!("{n:.1}");
    s.strip_suffix(".0")
        .map_or_else(|| s.clone(), ToString::to_string)
}

/// The temp-file stem `webfetch-${Date.now()}-${Math.random().toString(36).
/// slice(2,8)}` — a millisecond timestamp plus a 6-char base-36 suffix. The
/// on-disk filename is `<stem>.<ext>`.
///
/// `unix_ms` is the timestamp; `rand_seed` seeds the 6-char base-36 suffix (so the
/// suffix is deterministic in tests). Production passes the current time + a fresh
/// random seed.
#[must_use]
pub fn persisted_filename(unix_ms: u128, rand_seed: u64) -> String {
    format!("webfetch-{unix_ms}-{}", base36_6(rand_seed))
}

/// 6 lowercase base-36 chars from `seed` (`Math.random().toString(36).slice(2,8)`
/// yields 6 chars from the fractional part; we derive 6 chars from a 64-bit seed).
fn base36_6(seed: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut s = seed;
    let mut out = [0u8; 6];
    for slot in &mut out {
        *slot = DIGITS[(s % 36) as usize];
        s /= 36;
    }
    // Reverse so the high-order digit leads (cosmetic; any 6 base-36 chars are
    // valid — the suffix is only a uniqueness token).
    out.reverse();
    String::from_utf8(out.to_vec()).expect("base-36 digits are ASCII")
}

/// Outcome of [`persist_binary_content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistResult {
    /// Bytes written; `filepath` is the absolute path, `size` the byte count.
    Ok {
        /// Absolute path the bytes were written to (`<stem>.<ext>`).
        filepath: String,
        /// Number of bytes written.
        size: usize,
    },
    /// Write failed; `error` carries the message. The caller skips the footer on
    /// error (`if(!("error"in h))`).
    Err {
        /// Human-readable failure reason.
        error: String,
    },
}

/// `B$e(e,t,n)`: write the raw binary `bytes` to `<output_dir>/<stem>.<ext>`,
/// where `ext` is derived from `content_type` ([`mime_extension`]). The directory
/// is created (recursively) if missing (`Hhe()` / `mkdir`). Returns the path +
/// size, or an error string on failure (so the caller can skip the footer).
///
/// Mirrors the binary's `await ci().writeBytes(o,e)` — LingXi writes via
/// `std::fs` directly (the same in-process approach as `tools/file/pdf_render`
/// and `tools/mcp`'s `persistBinaryContent`), since `tool-web` runs in-process on
/// the desktop/posix targets where WebFetch fetches binary bodies. The
/// `tengu_binary_content_persisted` analytics event has no behavioral effect and
/// is omitted.
#[must_use]
pub fn persist_binary_content(
    bytes: &[u8],
    content_type: &str,
    stem: &str,
    output_dir: &Path,
) -> PersistResult {
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return PersistResult::Err {
            error: e.to_string(),
        };
    }
    let ext = mime_extension(content_type);
    let mut filepath: PathBuf = output_dir.to_path_buf();
    filepath.push(format!("{stem}.{ext}"));
    match std::fs::write(&filepath, bytes) {
        Ok(()) => PersistResult::Ok {
            filepath: filepath.to_string_lossy().into_owned(),
            size: bytes.len(),
        },
        Err(e) => PersistResult::Err {
            error: e.to_string(),
        },
    }
}

/// `if(A)_+=\`\n\n[Binary content (${f}, ${Ma(h??d)}) also saved to ${A}]\``:
/// when `persisted_path` is set, append the footer to `result`. The size shown is
/// `persisted_size ?? bytes` ([`human_size`]). When `persisted_path` is `None`,
/// `result` is returned unchanged.
#[must_use]
pub fn append_binary_footer(
    mut result: String,
    persisted_path: Option<&str>,
    content_type: &str,
    persisted_size: Option<usize>,
    bytes: usize,
) -> String {
    if let Some(path) = persisted_path {
        let size = persisted_size.unwrap_or(bytes) as u64;
        result.push_str(&format!(
            "\n\n[Binary content ({content_type}, {}) also saved to {path}]",
            human_size(size)
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- U7r (binary predicate) -------------------------------------------

    #[test]
    fn binary_predicate_text_and_structured_are_not_binary() {
        // Empty → not binary.
        assert!(!is_binary_content_type(""));
        // text/* → not binary (with a charset param).
        assert!(!is_binary_content_type("text/html; charset=utf-8"));
        assert!(!is_binary_content_type("TEXT/Plain"));
        // JSON family.
        assert!(!is_binary_content_type("application/json"));
        assert!(!is_binary_content_type("application/vnd.api+json"));
        // XML family.
        assert!(!is_binary_content_type("application/xml"));
        assert!(!is_binary_content_type("image/svg+xml"));
        // JavaScript + form-encoded.
        assert!(!is_binary_content_type(
            "application/javascript; charset=utf-8"
        ));
        assert!(!is_binary_content_type("application/x-www-form-urlencoded"));
    }

    #[test]
    fn binary_predicate_true_for_real_binaries() {
        assert!(is_binary_content_type("application/pdf"));
        assert!(is_binary_content_type("image/png"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(is_binary_content_type("application/zip"));
        assert!(is_binary_content_type("audio/mpeg"));
        // Unknown type → binary (the default `return !0`).
        assert!(is_binary_content_type("application/x-custom-thing"));
    }

    // ---- iZi (extension) ---------------------------------------------------

    #[test]
    fn extension_known_and_unknown() {
        assert_eq!(mime_extension("application/pdf"), "pdf");
        assert_eq!(mime_extension("image/jpeg"), "jpg");
        assert_eq!(mime_extension("text/markdown; charset=utf-8"), "md");
        assert_eq!(mime_extension("image/svg+xml"), "svg");
        assert_eq!(mime_extension(""), "bin");
        assert_eq!(mime_extension("application/octet-stream"), "bin");
    }

    // ---- Ma (human size) ---------------------------------------------------

    #[test]
    fn human_size_matches_binary() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(900), "900 bytes");
        assert_eq!(human_size(1024), "1KB");
        assert_eq!(human_size(1536), "1.5KB");
        assert_eq!(human_size(1_048_576), "1MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1GB");
        // `.0` trimmed: 2 MB exactly → "2MB", not "2.0MB".
        assert_eq!(human_size(2 * 1024 * 1024), "2MB");
    }

    // ---- filename ----------------------------------------------------------

    #[test]
    fn filename_shape() {
        let name = persisted_filename(1_700_000_000_000, 0xDEAD_BEEF);
        assert!(name.starts_with("webfetch-1700000000000-"));
        let suffix = name.rsplit('-').next().unwrap();
        assert_eq!(suffix.len(), 6);
        assert!(suffix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    // ---- B$e (persist) -----------------------------------------------------

    #[test]
    fn persist_writes_with_extension() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"\x89PNG\r\n\x1a\n binary";
        let res = persist_binary_content(bytes, "image/png", "webfetch-1-abc123", dir.path());
        match res {
            PersistResult::Ok { filepath, size } => {
                assert!(filepath.ends_with("webfetch-1-abc123.png"));
                assert_eq!(size, bytes.len());
                assert_eq!(std::fs::read(&filepath).unwrap(), bytes);
            }
            PersistResult::Err { error } => panic!("persist failed: {error}"),
        }
    }

    // ---- footer ------------------------------------------------------------

    #[test]
    fn footer_appended_when_persisted() {
        let out = append_binary_footer(
            "model summary".to_string(),
            Some("/tmp/tool-results/webfetch-1-abc123.pdf"),
            "application/pdf",
            Some(2_097_152),
            999,
        );
        assert_eq!(
            out,
            "model summary\n\n[Binary content (application/pdf, 2MB) also saved to /tmp/tool-results/webfetch-1-abc123.pdf]"
        );
    }

    #[test]
    fn footer_uses_bytes_when_persisted_size_absent() {
        let out = append_binary_footer(
            "x".to_string(),
            Some("/p/webfetch-2-def456.bin"),
            "application/octet-stream",
            None,
            1536,
        );
        assert_eq!(
            out,
            "x\n\n[Binary content (application/octet-stream, 1.5KB) also saved to /p/webfetch-2-def456.bin]"
        );
    }

    #[test]
    fn footer_noop_when_not_persisted() {
        let out = append_binary_footer("raw".to_string(), None, "text/html", None, 10);
        assert_eq!(out, "raw");
    }
}
