//! File encoding + line-ending detection and preservation for the Edit tool.
//!
//! Ported from claude-code:
//! - `utils/fileRead.ts` (`LineEndingType`, `detectEncodingForResolvedPath`,
//!   `detectLineEndingsForString`, `readFileSyncWithMetadata`),
//! - `tools/FileEditTool/FileEditTool.ts:202-221` (BOM→utf16le/utf8 +
//!   `replaceAll('\r\n','\n')` for matching),
//! - `utils/file.ts writeTextContent` (re-apply CRLF on write).
//!
//! The goal is that editing a CRLF or UTF-16LE file round-trips without
//! corruption and that `old_string` can match across a line break: callers
//! read into an LF-normalized in-memory string, match/replace on that, then
//! write back re-applying the file's ORIGINAL encoding + line endings.
//!
//! # Divergences from TS (1:1 notes)
//! - **UTF-16BE is NOT handled.** TS only inspects the little-endian BOM
//!   `FF FE` (`FileEditTool.ts:209-213` / `fileRead.ts:34`); a big-endian
//!   `FE FF` BOM is treated as UTF-8 there, so we match that exactly. (A
//!   leading `FE FF` is therefore decoded as lossy UTF-8, same as TS.)
//! - **UTF-8 BOM (`EF BB BF`).** TS classifies it as `utf8` and reads via
//!   Node's `'utf8'` decoder, which does NOT strip the BOM — the U+FEFF
//!   survives into `content`. We mirror that: encoding is `Utf8` and the BOM
//!   bytes are decoded as the U+FEFF char (preserved through round-trip).
//! - **Line-ending classification adds `Cr`.** TS `detectLineEndingsForString`
//!   only ever returns `'CRLF' | 'LF'` (it counts CRLF vs bare-LF and never
//!   classifies a CR-only file — such a file decodes to `'LF'` and is written
//!   back with its lone `\r`s lost). We additionally count bare `\r` and can
//!   classify a dominant-CR file as `Cr`, preserving it on write. For the
//!   CRLF-vs-LF tie-break we match TS exactly (`crlf > lf` ⇒ CRLF, else LF),
//!   so any file TS would call CRLF/LF we also call CRLF/LF.
//! - **Detection sample size.** TS samples the first 4096 code units of the
//!   decoded head for line-ending detection (`fileRead.ts:92`). We scan the
//!   full decoded content; for the dominant-ending decision this only differs
//!   on pathological files whose ending style flips after 4 KB, which the
//!   parity fixtures do not exercise.

use std::io;
use std::path::Path;

/// Text encoding detected from (or to be applied to) a file's bytes.
///
/// Mirrors the subset of Node `BufferEncoding` that claude-code's Edit path
/// distinguishes: UTF-8 (the default) and little-endian UTF-16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8 (default; also covers ASCII and a leading UTF-8 BOM).
    Utf8,
    /// Little-endian UTF-16, signalled by a leading `FF FE` BOM.
    Utf16Le,
}

/// Line-ending style detected from (or to be applied to) a file's content.
///
/// `Crlf`/`Lf` match TS `LineEndingType`; `Cr` is a Rust-only extension (see
/// the module-doc divergence note).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// `\n` — the LF-normalized in-memory form is written back as-is.
    Lf,
    /// `\r\n` — Windows-style.
    Crlf,
    /// `\r` — classic Mac-style (TS never produces this; see module doc).
    Cr,
}

impl LineEnding {
    /// The byte sequence this ending writes for each logical newline.
    fn sequence(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
            LineEnding::Cr => "\r",
        }
    }
}

/// Decode `bytes` into an LF-normalized string given the detected encoding.
///
/// For `Utf16Le` the leading `FF FE` BOM (2 bytes) is consumed and the
/// remaining little-endian `u16` code units are decoded manually (no external
/// crate). For `Utf8` the bytes are decoded lossily (matching Node's tolerant
/// decode — invalid sequences become U+FFFD rather than erroring).
fn decode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Encoding::Utf16Le => {
            // Strip the leading FF FE BOM if present (detection guarantees it
            // for `Utf16Le`, but guard anyway).
            let body = if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
                &bytes[2..]
            } else {
                bytes
            };
            // Read little-endian u16 code units; a trailing odd byte is
            // dropped (Node would also be unable to form a code unit from it).
            let units: Vec<u16> = body
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            // Lossy UTF-16 decode: unpaired surrogates become U+FFFD.
            String::from_utf16_lossy(&units)
        }
    }
}

/// Normalize CRLF and lone CR to LF in `content` (matching TS
/// `replaceAll('\r\n','\n')` plus our CR handling).
///
/// TS only collapses `\r\n`; we also map a bare `\r` to `\n` so that a
/// CR-only file presents a clean LF view for matching (the original ending is
/// re-applied on write via [`LineEnding`]).
fn normalize_to_lf(content: &str) -> String {
    // First collapse CRLF, then any remaining lone CR.
    content.replace("\r\n", "\n").replace('\r', "\n")
}

/// Count CRLF / bare-LF / bare-CR occurrences in `content`.
///
/// `crlf` increments on `\n` immediately preceded by `\r`; `lf` on any other
/// `\n`; `cr` on a `\r` NOT immediately followed by `\n`. This matches TS
/// `detectLineEndingsForString` for the CRLF/LF counts (`fileRead.ts:51-66`)
/// and adds the bare-CR count.
fn count_endings(content: &str) -> (usize, usize, usize) {
    let bytes = content.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut cr = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                if i > 0 && bytes[i - 1] == b'\r' {
                    crlf += 1;
                } else {
                    lf += 1;
                }
            }
            b'\r' => {
                // Bare CR only when not part of a CRLF pair.
                if i + 1 >= bytes.len() || bytes[i + 1] != b'\n' {
                    cr += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    (crlf, lf, cr)
}

/// Classify the dominant line ending of `content`.
///
/// TS-faithful for the CRLF-vs-LF decision (`crlf > lf` ⇒ `Crlf`, else `Lf`).
/// `Cr` is only chosen when bare CRs strictly dominate both CRLF and LF (a
/// case TS never produces — see module doc).
fn detect_line_ending(content: &str) -> LineEnding {
    let (crlf, lf, cr) = count_endings(content);
    // CR wins only if it strictly dominates both others (Rust-only extension).
    if cr > crlf && cr > lf {
        return LineEnding::Cr;
    }
    // TS tie-break: CRLF iff crlf > lf, else LF.
    if crlf > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// Detect a file's encoding from its leading bytes.
///
/// Matches TS `detectEncodingForResolvedPath` / the inline BOM check in
/// `FileEditTool.ts:208-213`: a leading `FF FE` ⇒ UTF-16LE, everything else
/// (including empty, a UTF-8 BOM, or a big-endian `FE FF`) ⇒ UTF-8.
fn detect_encoding(bytes: &[u8]) -> Encoding {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        Encoding::Utf16Le
    } else {
        Encoding::Utf8
    }
}

/// Read raw `bytes` into an LF-normalized content string plus the detected
/// encoding and line-ending style — the Rust analogue of TS
/// `readFileSyncWithMetadata` (`fileRead.ts:75-98`).
///
/// The returned `content` has CRLF (and lone CR) collapsed to LF, so callers
/// can match `old_string` across line breaks; the [`Encoding`]/[`LineEnding`]
/// are fed back into [`write_with_metadata`] to round-trip the original bytes.
#[must_use]
pub fn read_with_metadata(bytes: &[u8]) -> (String, Encoding, LineEnding) {
    let encoding = detect_encoding(bytes);
    let decoded = decode(bytes, encoding);
    // Detect line endings from the decoded (pre-normalization) text.
    let line_ending = detect_line_ending(&decoded);
    let content = normalize_to_lf(&decoded);
    (content, encoding, line_ending)
}

/// Encode `content_lf` (an LF-normalized string) into bytes, re-applying
/// `ending` and `enc` — the Rust analogue of TS `writeTextContent`
/// (`file.ts:84-98`) combined with the encoding re-application.
///
/// For `Crlf`/`Cr` every `\n` becomes the original ending. TS only re-applies
/// CRLF (it leaves LF content untouched and never produces CR); we additionally
/// re-apply CR so a CR-only file round-trips.
#[must_use]
pub fn encode_with_metadata(content_lf: &str, enc: Encoding, ending: LineEnding) -> Vec<u8> {
    // Re-apply line endings. `content_lf` is already LF-normalized, but guard
    // against stray CRLF in model output the same way TS does (collapse first,
    // then expand) so we never emit `\r\r\n`.
    let normalized = normalize_to_lf(content_lf);
    let with_endings = match ending {
        LineEnding::Lf => normalized,
        LineEnding::Crlf | LineEnding::Cr => normalized.replace('\n', ending.sequence()),
    };

    match enc {
        Encoding::Utf8 => with_endings.into_bytes(),
        Encoding::Utf16Le => {
            // Emit the FF FE BOM followed by little-endian u16 code units.
            let mut out = Vec::with_capacity(2 + with_endings.len() * 2);
            out.push(0xFF);
            out.push(0xFE);
            for unit in with_endings.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            out
        }
    }
}

/// Write `content_lf` to `path`, re-applying the original `enc`/`ending`.
///
/// Thin wrapper over [`encode_with_metadata`] + a blocking write. Edit uses
/// the async wrapper in `edit.rs`; this synchronous helper exists for direct
/// callers and tests.
pub fn write_with_metadata(
    path: &Path,
    content_lf: &str,
    enc: Encoding,
    ending: LineEnding,
) -> io::Result<()> {
    std::fs::write(path, encode_with_metadata(content_lf, enc, ending))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_lf_plain() {
        let (content, enc, ending) = read_with_metadata(b"a\nb\nc\n");
        assert_eq!(content, "a\nb\nc\n");
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(ending, LineEnding::Lf);
    }

    #[test]
    fn detects_crlf() {
        let (content, enc, ending) = read_with_metadata(b"a\r\nb\r\nc\r\n");
        // In-memory view is LF-normalized.
        assert_eq!(content, "a\nb\nc\n");
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(ending, LineEnding::Crlf);
    }

    #[test]
    fn detects_cr_only() {
        let (content, _enc, ending) = read_with_metadata(b"a\rb\rc\r");
        assert_eq!(content, "a\nb\nc\n");
        assert_eq!(ending, LineEnding::Cr);
    }

    #[test]
    fn mixed_endings_pick_dominant_crlf() {
        // 3 CRLF vs 1 bare LF ⇒ CRLF dominates (matches TS crlf > lf).
        let (_content, _enc, ending) = read_with_metadata(b"a\r\nb\r\nc\r\nd\ne");
        assert_eq!(ending, LineEnding::Crlf);
    }

    #[test]
    fn mixed_endings_tie_goes_to_lf() {
        // 1 CRLF vs 1 LF — TS tie-break: crlf > lf is false ⇒ LF.
        let (_content, _enc, ending) = read_with_metadata(b"a\r\nb\nc");
        assert_eq!(ending, LineEnding::Lf);
    }

    #[test]
    fn utf16le_bom_detected_and_decoded() {
        // "Hi" in UTF-16LE with BOM: FF FE 48 00 69 00
        let bytes = [0xFF, 0xFE, 0x48, 0x00, 0x69, 0x00];
        let (content, enc, ending) = read_with_metadata(&bytes);
        assert_eq!(content, "Hi");
        assert_eq!(enc, Encoding::Utf16Le);
        assert_eq!(ending, LineEnding::Lf);
    }

    #[test]
    fn utf16le_crlf_round_trip() {
        // "a\r\nb" in UTF-16LE with BOM.
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "a\r\nb".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let (content, enc, ending) = read_with_metadata(&bytes);
        assert_eq!(content, "a\nb");
        assert_eq!(enc, Encoding::Utf16Le);
        assert_eq!(ending, LineEnding::Crlf);
        // Round-trip the unchanged content back to identical bytes.
        let out = encode_with_metadata(&content, enc, ending);
        assert_eq!(out, bytes);
    }

    #[test]
    fn utf16be_bom_treated_as_utf8() {
        // FE FF (big-endian BOM) is NOT recognized by TS ⇒ Utf8.
        let bytes = [0xFE, 0xFF, 0x00, 0x41];
        let (_content, enc, _ending) = read_with_metadata(&bytes);
        assert_eq!(enc, Encoding::Utf8);
    }

    #[test]
    fn utf8_bom_preserved_as_char() {
        // EF BB BF is classified utf8 and the U+FEFF survives (Node-faithful).
        let bytes = [0xEF, 0xBB, 0xBF, b'x'];
        let (content, enc, _ending) = read_with_metadata(&bytes);
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(content, "\u{FEFF}x");
    }

    #[test]
    fn empty_file_is_utf8_lf() {
        let (content, enc, ending) = read_with_metadata(b"");
        assert!(content.is_empty());
        assert_eq!(enc, Encoding::Utf8);
        assert_eq!(ending, LineEnding::Lf);
    }

    #[test]
    fn encode_crlf_reapplies_endings() {
        let out = encode_with_metadata("a\nb\nc", Encoding::Utf8, LineEnding::Crlf);
        assert_eq!(out, b"a\r\nb\r\nc");
    }

    #[test]
    fn encode_crlf_collapses_existing_crlf_first() {
        // Model output already containing \r\n must not become \r\r\n.
        let out = encode_with_metadata("a\r\nb", Encoding::Utf8, LineEnding::Crlf);
        assert_eq!(out, b"a\r\nb");
    }

    #[test]
    fn encode_lf_is_passthrough() {
        let out = encode_with_metadata("a\nb", Encoding::Utf8, LineEnding::Lf);
        assert_eq!(out, b"a\nb");
    }

    #[test]
    fn encode_cr_reapplies() {
        let out = encode_with_metadata("a\nb", Encoding::Utf8, LineEnding::Cr);
        assert_eq!(out, b"a\rb");
    }

    #[test]
    fn write_with_metadata_writes_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("f.txt");
        write_with_metadata(&p, "x\ny", Encoding::Utf8, LineEnding::Crlf).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"x\r\ny");
    }
}
