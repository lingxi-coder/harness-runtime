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
//! Claude Code 2.1.322 preserves BOMs in the decoded view, normalizes CRLF
//! only, and samples the first 4096 UTF-16 units for line-ending detection.
//! Mutations reject unsupported encodings before they can replace their bytes.

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

/// Native `got` classifications. A UTF-16LE BOM is the only binary-looking
/// prefix which the mutation tools accept as writable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotText {
    NotUtf8,
    Nul,
    Utf16Be,
}

impl NotText {
    fn description(self) -> &'static str {
        match self {
            Self::NotUtf8 => "is not valid UTF-8: it uses a legacy encoding such as Windows-1252, Shift-JIS or GBK, or it is binary",
            Self::Nul => "has NUL bytes in its first 8 KB: binary data, UTF-16/UTF-32 without a byte-order mark, or text with embedded NULs",
            Self::Utf16Be => "is UTF-16 big-endian (it starts FE FF), which these tools cannot write",
        }
    }

    #[must_use]
    pub fn mutation_refusal_message(self) -> String {
        format!("File {}. These tools write UTF-8 (or UTF-16LE behind its byte-order mark) and never convert a file's encoding on their own, so nothing was written. If the file is text, make the change with a shell command that reads and writes it in its own encoding (iconv, or PowerShell Get-Content/Set-Content -Encoding), or ask the user whether to convert it to UTF-8 first. To replace the whole file with new UTF-8 text anyway, delete it first, then Write it.", self.description())
    }

    #[must_use]
    pub fn read_warning(self) -> String {
        let undecodable = if self == Self::NotUtf8 {
            " Its undecodable bytes show above as U+FFFD."
        } else {
            ""
        };
        format!("[This file {}.{undecodable} Edit and Write will refuse it: change it with a shell command that keeps its encoding, or ask the user whether to convert it to UTF-8.]", self.description())
    }
}

#[must_use]
pub fn classify_not_text(bytes: &[u8]) -> Option<NotText> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        None
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Some(NotText::Utf16Be)
    } else if bytes[..bytes.len().min(8000)].contains(&0) {
        Some(NotText::Nul)
    } else if std::str::from_utf8(bytes).is_err() {
        Some(NotText::NotUtf8)
    } else {
        None
    }
}

/// Native `gjr`: retain an existing BOM for a nonempty whole-file replacement.
#[must_use]
pub fn preserve_bom<'a>(previous: &str, replacement: &'a str) -> std::borrow::Cow<'a, str> {
    if !replacement.is_empty()
        && previous.starts_with('\u{feff}')
        && !replacement.starts_with('\u{feff}')
    {
        format!("\u{feff}{replacement}").into()
    } else {
        replacement.into()
    }
}

/// Line-ending style detected from (or to be applied to) a file's content.
///
/// `Crlf`/`Lf` match TS `LineEndingType`. `Cr` is retained for callers that
/// explicitly request it; native detection never selects it.
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

/// Decode `bytes` with Node-compatible encoding selection, retaining its BOM.
///
/// UTF-16LE code units include the leading BOM; an odd trailing byte is ignored.
/// UTF-8 decoding replaces malformed sequences for Read diagnostics. Mutations
/// use `classify_not_text` to reject those bytes before writing.
fn decode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Encoding::Utf16Le => {
            // Read little-endian u16 code units; a trailing odd byte is
            // dropped (Node would also be unable to form a code unit from it).
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            // Lossy UTF-16 decode: unpaired surrogates become U+FFFD.
            String::from_utf16_lossy(&units)
        }
    }
}

/// Normalize only CRLF pairs; a lone CR is literal native file content.
fn normalize_to_lf(content: &str) -> String {
    content.replace("\r\n", "\n")
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
/// Only the first 4096 UTF-16 code units participate in detection.
fn detect_line_ending(content: &str) -> LineEnding {
    let sample_end = content
        .char_indices()
        .scan(0usize, |units, (index, ch)| {
            *units += ch.len_utf16();
            Some((index + ch.len_utf8(), *units))
        })
        .take_while(|(_, units)| *units <= 4096)
        .last()
        .map_or(0, |(end, _)| end);
    let (crlf, lf, _) = count_endings(&content[..sample_end]);
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

/// Node's BOM-selected text decoding before line-ending normalization.
#[must_use]
pub fn decode_native_text(bytes: &[u8]) -> String {
    decode(bytes, detect_encoding(bytes))
}

/// Read raw `bytes` into an LF-normalized content string plus the detected
/// encoding and line-ending style — the Rust analogue of TS
/// `readFileSyncWithMetadata` (`fileRead.ts:75-98`).
///
/// The returned `content` has CRLF collapsed to LF, so callers
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
    // Native BUe leaves LF mode untouched, and collapses then expands CRLF
    // only when the original file selected that ending.
    let with_endings = match ending {
        LineEnding::Lf => content_lf.to_owned(),
        LineEnding::Crlf | LineEnding::Cr => {
            normalize_to_lf(content_lf).replace('\n', ending.sequence())
        }
    };

    match enc {
        Encoding::Utf8 => with_endings.into_bytes(),
        Encoding::Utf16Le => {
            let mut out = Vec::with_capacity(with_endings.len() * 2);
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

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/file_mutation_322.json"
        ))
        .unwrap()
    }

    fn hex_bytes(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn native_322_encoding_metadata_and_write_bytes() {
        let fixture = oracle();
        for case in fixture["classifications"].as_array().unwrap() {
            let bytes = hex_bytes(case["inputHex"].as_str().unwrap());
            let kind = classify_not_text(&bytes);
            let expected = &case["expected"];
            let name = case["name"].as_str().unwrap();
            let kind_name = kind.map(|kind| match kind {
                NotText::NotUtf8 => "not_utf8",
                NotText::Nul => "nul",
                NotText::Utf16Be => "utf16be",
            });
            assert_eq!(kind_name, expected["notText"].as_str(), "{name}");
            assert_eq!(
                kind.map(NotText::mutation_refusal_message).as_deref(),
                expected["refusal"].as_str(),
                "{name}"
            );
            assert_eq!(
                kind.map(NotText::read_warning).as_deref(),
                expected["warning"].as_str(),
                "{name}"
            );
        }
        for case in fixture["metadata"].as_array().unwrap() {
            let (content, encoding, ending) =
                read_with_metadata(&hex_bytes(case["inputHex"].as_str().unwrap()));
            let expected = &case["expected"];
            let name = case["name"].as_str().unwrap();
            assert_eq!(content, expected["content"].as_str().unwrap(), "{name}");
            assert_eq!(
                match encoding {
                    Encoding::Utf8 => "utf8",
                    Encoding::Utf16Le => "utf16le",
                },
                expected["encoding"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                match ending {
                    LineEnding::Crlf => "CRLF",
                    _ => "LF",
                },
                expected["lineEndings"].as_str().unwrap(),
                "{name}"
            );
        }
        for case in fixture["writes"].as_array().unwrap() {
            let bytes = hex_bytes(case["originalHex"].as_str().unwrap());
            let (original, encoding, _) = read_with_metadata(&bytes);
            let content = preserve_bom(&original, case["replacement"].as_str().unwrap());
            let expected = &case["expected"];
            assert_eq!(
                original,
                expected["originalFile"].as_str().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                content.as_ref(),
                expected["content"].as_str().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                encode_with_metadata(&content, encoding, LineEnding::Lf),
                hex_bytes(expected["bytesHex"].as_str().unwrap()),
                "{}",
                case["name"]
            );
        }
    }

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
        assert_eq!(content, "a\rb\rc\r");
        assert_eq!(ending, LineEnding::Lf);
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
        assert_eq!(content, "\u{feff}Hi");
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
        assert_eq!(content, "\u{feff}a\nb");
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
