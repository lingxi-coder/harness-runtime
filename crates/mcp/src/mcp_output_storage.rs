//! Binary-blob persistence for MCP `resources/read` (MCP-5d).
//!
//! 1:1 port of claude-code `src/utils/mcpOutputStorage.ts` (the bits the
//! `ReadMcpResourceTool` actually uses) plus `formatFileSize`
//! (`src/utils/format.ts:9-23`).
//!
//! When an MCP resource read returns a base64 `blob` content block, the raw
//! base64 must NOT be stringified straight into the model context. Instead the
//! bytes are decoded and written to disk with a mime-derived extension, and the
//! model is handed the path (so it can open the file with native tooling — Read
//! for PDFs, pandas for xlsx, etc.).
//!
//! The write goes through `std::fs` directly (mirroring TS `writeFile`) rather
//! than the sandboxed `FileSystem` trait, because that trait only writes UTF-8
//! `&str` and cannot carry raw binary bytes. The `output_dir` is supplied by
//! the caller (a session/tool-results dir in production, a `tempfile::TempDir`
//! in tests) so no real home directory is ever written during tests.

use lingxi_core::host::McpResourceContentsRich;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One element of a `resources/read` `contents[]` array as decoded off the
/// wire, before blob persistence. `text` and `blob` are mutually exclusive in
/// the MCP spec (a content block is one or the other); both absent is an
/// empty/opaque block.
#[derive(Debug, Clone)]
pub struct RawResourceContent {
    /// Echoed URI for this content block (the request URI is substituted when
    /// the server omits one).
    pub uri: String,
    /// MIME type as advertised by the server.
    pub mime_type: Option<String>,
    /// Opaque vendor metadata preserved from the wire.
    pub meta: Option<Value>,
    /// UTF-8 text body, for text content blocks.
    pub text: Option<String>,
    /// base64-encoded body, for binary blob content blocks.
    pub blob: Option<String>,
}

/// Map the raw `contents[]` of a `resources/read` response into the rich
/// claude-code shape. 1:1 with `ReadMcpResourceTool.ts:106-139`:
///
/// * a `text` block → `{uri, mimeType, text}` verbatim;
/// * a block with neither `text` nor a string `blob` → `{uri, mimeType}` only;
/// * a `blob` block → decode base64, persist the bytes via
///   [`persist_binary_content`], and return `{uri, mimeType, blobSavedTo, text}`
///   where `text` is [`binary_blob_saved_message`]. A decode/persist failure
///   degrades to `{uri, mimeType, text: "Binary content could not be saved…"}`
///   (no `blobSavedTo`), exactly like the TS `'error' in persisted` branch.
///
/// `server_name` + each block's URI form the `sourceDescription`
/// `"[Resource from <server> at <uri>] "` prefix. `now_millis` and `rand_tag`
/// feed the `persistId` so blob filenames are unique per block; the caller
/// supplies them (production: wall clock + RNG; tests: fixed values).
#[must_use]
pub fn map_resource_contents(
    contents: Vec<RawResourceContent>,
    server_name: &str,
    output_dir: &Path,
    now_millis: u128,
    rand_tag: &str,
) -> Vec<McpResourceContentsRich> {
    contents
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            // Text block: pass through verbatim (TS `'text' in c`).
            if let Some(text) = c.text {
                return McpResourceContentsRich {
                    uri: c.uri,
                    mime_type: c.mime_type,
                    meta: c.meta,
                    text: Some(text),
                    blob_saved_to: None,
                };
            }
            // Not a string blob: `{uri, mimeType}` only.
            let Some(blob) = c.blob else {
                return McpResourceContentsRich {
                    uri: c.uri,
                    mime_type: c.mime_type,
                    meta: c.meta,
                    text: None,
                    blob_saved_to: None,
                };
            };
            // Blob block: decode + persist. `persistId` mirrors the TS template
            // `mcp-resource-${Date.now()}-${i}-${rand}`.
            let persist_id = format!("mcp-resource-{now_millis}-{i}-{rand_tag}");
            let source_description = format!("[Resource from {server_name} at {}] ", c.uri);
            let bytes = match decode_base64(&blob) {
                Ok(b) => b,
                Err(e) => {
                    return McpResourceContentsRich {
                        uri: c.uri,
                        mime_type: c.mime_type,
                        meta: c.meta,
                        text: Some(format!("Binary content could not be saved to disk: {e}")),
                        blob_saved_to: None,
                    };
                }
            };
            match persist_binary_content(&bytes, c.mime_type.as_deref(), &persist_id, output_dir) {
                PersistBinaryResult::Ok { filepath, size, .. } => {
                    let text = binary_blob_saved_message(
                        &filepath,
                        c.mime_type.as_deref(),
                        size,
                        &source_description,
                    );
                    McpResourceContentsRich {
                        uri: c.uri,
                        mime_type: c.mime_type,
                        meta: c.meta,
                        text: Some(text),
                        blob_saved_to: Some(filepath),
                    }
                }
                PersistBinaryResult::Err { error } => McpResourceContentsRich {
                    uri: c.uri,
                    mime_type: c.mime_type,
                    meta: c.meta,
                    text: Some(format!(
                        "Binary content could not be saved to disk: {error}"
                    )),
                    blob_saved_to: None,
                },
            }
        })
        .collect()
}

/// Map a mime type to a file extension. Conservative: known types get their
/// proper extension; unknown / absent types get `bin`. The extension matters
/// because the Read tool dispatches on it (PDFs, images, etc. need the right
/// ext). 1:1 with `mcpOutputStorage.ts:66-118` `extensionForMimeType`.
#[must_use]
pub fn extension_for_mime_type(mime_type: Option<&str>) -> &'static str {
    let Some(raw) = mime_type else {
        return "bin";
    };
    // Strip any charset/boundary parameter, then lowercase.
    let mt = raw
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

/// Format a byte count to a human-readable string. 1:1 with
/// `format.ts:9-23` `formatFileSize` — including the `.0`-trimming so
/// `formatFileSize(1536) == "1.5KB"` and `formatFileSize(1048576) == "1MB"`.
#[must_use]
pub fn format_file_size(size_in_bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let kb = size_in_bytes as f64 / 1024.0;
    if kb < 1.0 {
        return format!("{size_in_bytes} bytes");
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

/// `n.toFixed(1).replace(/\.0$/, '')` — one decimal place, then strip a
/// trailing `.0`. (`1.0 -> "1"`, `1.5 -> "1.5"`.)
fn trim_dot_zero(n: f64) -> String {
    let s = format!("{n:.1}");
    s.strip_suffix(".0").map_or(s.clone(), ToString::to_string)
}

/// Build the message telling the model where binary content was saved. 1:1 with
/// `mcpOutputStorage.ts:181-189` `getBinaryBlobSavedMessage`. The byte layout of
/// this string is locked — downstream parity tests match it verbatim.
#[must_use]
pub fn binary_blob_saved_message(
    filepath: &str,
    mime_type: Option<&str>,
    size: u64,
    source_description: &str,
) -> String {
    let mt = mime_type
        .filter(|m| !m.is_empty())
        .unwrap_or("unknown type");
    format!(
        "{source_description}Binary content ({mt}, {}) saved to {filepath}",
        format_file_size(size)
    )
}

/// Outcome of [`persist_binary_content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistBinaryResult {
    /// Bytes were written; `filepath` is the absolute path, `size` the byte
    /// count, `ext` the chosen extension.
    Ok {
        /// Absolute path the bytes were written to.
        filepath: String,
        /// Number of bytes written.
        size: u64,
        /// Mime-derived file extension (no leading dot).
        ext: &'static str,
    },
    /// Write failed; `error` carries the message (surfaced to the model).
    Err {
        /// Human-readable failure reason.
        error: String,
    },
}

/// Write raw binary `bytes` to `output_dir` with a mime-derived extension under
/// the name `<persist_id>.<ext>`. 1:1 with `mcpOutputStorage.ts:148-174`
/// `persistBinaryContent` (the `ensureToolResultsDir()` + `writeFile` core; the
/// analytics `logEvent` has no behavioral effect and is omitted).
///
/// `output_dir` is created (recursively) if missing.
#[must_use]
pub fn persist_binary_content(
    bytes: &[u8],
    mime_type: Option<&str>,
    persist_id: &str,
    output_dir: &Path,
) -> PersistBinaryResult {
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        return PersistBinaryResult::Err {
            error: e.to_string(),
        };
    }
    let ext = extension_for_mime_type(mime_type);
    let mut filepath: PathBuf = output_dir.to_path_buf();
    filepath.push(format!("{persist_id}.{ext}"));
    match std::fs::write(&filepath, bytes) {
        Ok(()) => PersistBinaryResult::Ok {
            filepath: filepath.to_string_lossy().into_owned(),
            size: bytes.len() as u64,
            ext,
        },
        Err(e) => PersistBinaryResult::Err {
            error: e.to_string(),
        },
    }
}

/// Decode a standard (non-URL-safe, RFC 4648) base64 string into bytes.
/// Hand-rolled to avoid adding a `base64` dependency to the `mcp` crate.
///
/// Tolerant of embedded ASCII whitespace (newlines/spaces), which MCP servers
/// sometimes insert. Rejects any other invalid character or a wrong-length
/// (mod-4) group with [`Base64Error`].
///
/// # Errors
/// Returns [`Base64Error`] for an invalid character, misplaced padding, or a
/// truncated final group.
pub fn decode_base64(input: &str) -> Result<Vec<u8>, Base64Error> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    // 6-bit accumulator: collect 4 sextets → 3 bytes.
    let mut quad = [0u8; 4];
    let mut n = 0usize; // sextets collected into `quad`
    let mut pad = 0usize; // '=' seen in the current quad
    for b in input.bytes() {
        match b {
            // Skip ASCII whitespace anywhere.
            b' ' | b'\n' | b'\r' | b'\t' | 0x0b | 0x0c => continue,
            b'=' => {
                pad += 1;
                quad[n] = 0;
                n += 1;
            }
            _ => {
                if pad > 0 {
                    // Data after padding is invalid.
                    return Err(Base64Error);
                }
                quad[n] = sextet(b)?;
                n += 1;
            }
        }
        if n == 4 {
            push_quad(&mut out, quad, pad)?;
            n = 0;
            pad = 0;
        }
    }
    if n != 0 {
        // A trailing partial group without padding. Accept only the canonical
        // unpadded forms (2 sextets → 1 byte, 3 sextets → 2 bytes); a lone
        // sextet is never valid.
        if n == 1 {
            return Err(Base64Error);
        }
        // Zero-fill the rest as implicit padding.
        let implied_pad = 4 - n;
        for slot in quad.iter_mut().skip(n) {
            *slot = 0;
        }
        push_quad(&mut out, quad, implied_pad)?;
    }
    Ok(out)
}

/// Translate one 4-sextet group (with `pad` trailing `=`) into 1-3 output bytes.
fn push_quad(out: &mut Vec<u8>, quad: [u8; 4], pad: usize) -> Result<(), Base64Error> {
    let n0 = quad[0];
    let n1 = quad[1];
    let n2 = quad[2];
    let n3 = quad[3];
    out.push((n0 << 2) | (n1 >> 4));
    match pad {
        0 => {
            out.push((n1 << 4) | (n2 >> 2));
            out.push((n2 << 6) | n3);
        }
        1 => {
            out.push((n1 << 4) | (n2 >> 2));
        }
        2 => {}
        // 3 or more '=' in a quad is malformed.
        _ => return Err(Base64Error),
    }
    Ok(())
}

/// Map one base64 alphabet character to its 6-bit value.
fn sextet(b: u8) -> Result<u8, Base64Error> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a' + 26),
        b'0'..=b'9' => Ok(b - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(Base64Error),
    }
}

/// Invalid base64 input encountered by [`decode_base64`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Base64Error;

impl std::fmt::Display for Base64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid base64 input")
    }
}

impl std::error::Error for Base64Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_file_size_matches_ts() {
        // Mirrors format.ts examples and boundaries.
        assert_eq!(format_file_size(0), "0 bytes");
        assert_eq!(format_file_size(512), "512 bytes");
        assert_eq!(format_file_size(1023), "1023 bytes");
        assert_eq!(format_file_size(1024), "1KB"); // .0 trimmed
        assert_eq!(format_file_size(1536), "1.5KB");
        assert_eq!(format_file_size(1024 * 1024), "1MB");
        assert_eq!(format_file_size(1024 * 1024 * 3 / 2), "1.5MB");
        assert_eq!(format_file_size(1024 * 1024 * 1024), "1GB");
    }

    #[test]
    fn extension_for_mime_type_known_and_unknown() {
        assert_eq!(extension_for_mime_type(Some("application/pdf")), "pdf");
        assert_eq!(extension_for_mime_type(Some("image/png")), "png");
        // charset parameter is stripped + case-insensitive.
        assert_eq!(
            extension_for_mime_type(Some("text/CSV; charset=utf-8")),
            "csv"
        );
        assert_eq!(
            extension_for_mime_type(Some("application/octet-stream")),
            "bin"
        );
        assert_eq!(extension_for_mime_type(None), "bin");
    }

    #[test]
    fn binary_blob_saved_message_byte_layout() {
        let msg = binary_blob_saved_message(
            "/tmp/out/mcp-resource-x.pdf",
            Some("application/pdf"),
            1536,
            "[Resource from fs at file:///doc] ",
        );
        assert_eq!(
            msg,
            "[Resource from fs at file:///doc] Binary content (application/pdf, 1.5KB) saved to /tmp/out/mcp-resource-x.pdf"
        );
        // Absent mime → "unknown type".
        let msg2 = binary_blob_saved_message("/p.bin", None, 10, "");
        assert_eq!(
            msg2,
            "Binary content (unknown type, 10 bytes) saved to /p.bin"
        );
    }

    #[test]
    fn decode_base64_round_trips() {
        // "hello" → "aGVsbG8=" (1 pad), "Man" → "TWFu" (no pad), "Ma" → "TWE=".
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("TWFu").unwrap(), b"Man");
        assert_eq!(decode_base64("TWE=").unwrap(), b"Ma");
        assert_eq!(decode_base64("TQ==").unwrap(), b"M");
        assert_eq!(decode_base64("").unwrap(), b"");
        // Whitespace tolerated.
        assert_eq!(decode_base64("aGVs\nbG8=").unwrap(), b"hello");
        // Unpadded canonical forms accepted.
        assert_eq!(decode_base64("TWE").unwrap(), b"Ma");
        // Full byte range round-trips.
        let bytes: Vec<u8> = (0u8..=255).collect();
        let encoded = encode_for_test(&bytes);
        assert_eq!(decode_base64(&encoded).unwrap(), bytes);
    }

    #[test]
    fn decode_base64_rejects_garbage() {
        assert!(decode_base64("====").is_err()); // padding-only quad
        assert!(decode_base64("A").is_err()); // lone sextet
        assert!(decode_base64("!!!!").is_err()); // invalid chars
        assert!(decode_base64("TW=u").is_err()); // data after padding
    }

    #[test]
    fn persist_binary_content_writes_bytes_and_returns_path() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"\x89PNG\r\n\x1a\n binary payload";
        let res = persist_binary_content(bytes, Some("image/png"), "mcp-resource-42", dir.path());
        match res {
            PersistBinaryResult::Ok {
                filepath,
                size,
                ext,
            } => {
                assert_eq!(ext, "png");
                assert_eq!(size, bytes.len() as u64);
                assert!(filepath.ends_with("mcp-resource-42.png"));
                let on_disk = std::fs::read(&filepath).unwrap();
                assert_eq!(on_disk, bytes, "raw bytes written verbatim");
            }
            PersistBinaryResult::Err { error } => panic!("expected Ok, got {error}"),
        }
    }

    #[test]
    fn map_text_block_passes_through_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let out = map_resource_contents(
            vec![RawResourceContent {
                uri: "mock://readme".into(),
                mime_type: Some("text/plain".into()),
                meta: Some(serde_json::json!({"openai/widgetDescription":"Readme"})),
                text: Some("hello from mock resource".into()),
                blob: None,
            }],
            "fs",
            dir.path(),
            123,
            "abc",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].uri, "mock://readme");
        assert_eq!(out[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(
            out[0].meta,
            Some(serde_json::json!({"openai/widgetDescription":"Readme"}))
        );
        assert_eq!(out[0].text.as_deref(), Some("hello from mock resource"));
        assert_eq!(out[0].blob_saved_to, None);
        // Nothing should have been written to disk for a pure text block.
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn map_blob_block_persists_and_sets_blob_saved_to() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"\x89PNG\r\n\x1a\nfake-png-bytes";
        let b64 = encode_for_test(payload);
        let out = map_resource_contents(
            vec![RawResourceContent {
                uri: "mock://image".into(),
                mime_type: Some("image/png".into()),
                meta: None,
                text: None,
                blob: Some(b64),
            }],
            "imgsrv",
            dir.path(),
            1700,
            "z9",
        );
        assert_eq!(out.len(), 1);
        let entry = &out[0];
        assert_eq!(entry.uri, "mock://image");
        assert_eq!(entry.mime_type.as_deref(), Some("image/png"));
        let saved = entry.blob_saved_to.as_deref().expect("blob persisted");
        // persistId template: mcp-resource-<now>-<i>-<rand>.<ext>
        assert!(saved.ends_with("mcp-resource-1700-0-z9.png"), "got {saved}");
        // Bytes on disk are the raw decoded payload (NOT base64).
        assert_eq!(std::fs::read(saved).unwrap(), payload);
        // text carries the exact getBinaryBlobSavedMessage line.
        assert_eq!(
            entry.text.as_deref().unwrap(),
            format!(
                "[Resource from imgsrv at mock://image] Binary content (image/png, {}) saved to {saved}",
                format_file_size(payload.len() as u64)
            )
        );
    }

    #[test]
    fn map_blob_with_invalid_base64_degrades_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let out = map_resource_contents(
            vec![RawResourceContent {
                uri: "mock://bad".into(),
                mime_type: Some("application/pdf".into()),
                meta: None,
                text: None,
                blob: Some("!!not-base64!!".into()),
            }],
            "fs",
            dir.path(),
            1,
            "r",
        );
        assert_eq!(out[0].blob_saved_to, None);
        assert!(out[0]
            .text
            .as_deref()
            .unwrap()
            .starts_with("Binary content could not be saved to disk:"));
    }

    #[test]
    fn map_empty_block_yields_uri_and_mime_only() {
        let dir = tempfile::tempdir().unwrap();
        let out = map_resource_contents(
            vec![RawResourceContent {
                uri: "mock://opaque".into(),
                mime_type: Some("application/octet-stream".into()),
                meta: None,
                text: None,
                blob: None,
            }],
            "fs",
            dir.path(),
            1,
            "r",
        );
        assert_eq!(out[0].uri, "mock://opaque");
        assert_eq!(
            out[0].mime_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(out[0].text, None);
        assert_eq!(out[0].blob_saved_to, None);
    }

    #[test]
    fn map_multi_content_mixes_text_and_blob() {
        let dir = tempfile::tempdir().unwrap();
        let out = map_resource_contents(
            vec![
                RawResourceContent {
                    uri: "mock://a".into(),
                    mime_type: Some("text/markdown".into()),
                    meta: None,
                    text: Some("# title".into()),
                    blob: None,
                },
                RawResourceContent {
                    uri: "mock://b".into(),
                    mime_type: Some("application/pdf".into()),
                    meta: None,
                    text: None,
                    blob: Some(encode_for_test(b"%PDF-1.4 body")),
                },
            ],
            "srv",
            dir.path(),
            42,
            "q",
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text.as_deref(), Some("# title"));
        assert_eq!(out[0].blob_saved_to, None);
        // Second element used index i=1 in its persistId.
        let saved = out[1].blob_saved_to.as_deref().unwrap();
        assert!(saved.ends_with("mcp-resource-42-1-q.pdf"), "got {saved}");
    }

    /// Minimal standard base64 encoder for test fixtures only.
    fn encode_for_test(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as usize;
            let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
            let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
            out.push(ALPHABET[b0 >> 2] as char);
            out.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[b2 & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}
