//! `FileReadTool` — read a UTF-8 file with size guard + binary detection.
//!
//! Wire-locked constants:
//! - `MAX_FILE_READ_SIZE = 262_144` bytes (256 KB) per spec §7.
//! - Binary detection scans first `NUL_SCAN_WINDOW = 8 * 1024` bytes for NUL.
//! - Default encoding UTF-8 (BOM-aware) per spec §7.
//! - 1-based line indexing on tool input/output per spec §7.
//! - Errors carry byte-locked human strings (see [`format_too_large`],
//!   [`format_binary`]).

use crate::shared::{decode_utf8_strict, looks_binary, NUL_SCAN_WINDOW};
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    FILE_READ_DEDUP, FILE_READ_LIMITS_OVERRIDE, FILE_READ_REREAD, READ_COMPLETED, READ_FAILED,
    READ_STARTED, SESSION_FILE_READ,
};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::util::ids::ulid_or_uuid;
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, resolve_against_cwd, translate_model_path,
};
use tool_api::BuiltinToolContext;

/// Maximum file size FileReadTool will load. Spec §7 lock (256 KB).
pub const MAX_FILE_READ_SIZE: u64 = 262_144;

/// Prompt-advertised default line count — byte-locked to claude-code `LQe`
/// (`FileReadTool/prompt.ts:10`, the "it reads up to 2000 lines" wording). This
/// is a PROMPT-TEXT constant ONLY: claude-code's runtime applies NO 2000-line
/// cap. When the caller supplies no `limit`, `readFileInRange` is called with
/// `maxLines = limit` VERBATIM (the `?? MAX_LINES_TO_READ` default does NOT
/// exist) — `maxLines === undefined` reads to EOF, and an over-budget full read
/// is gracefully token-truncated (see [`truncate_to_token_budget`]) rather than
/// line-capped. Used solely to interpolate the prompt description below.
pub const MAX_LINES_TO_READ: u64 = 2000;

/// Default per-read output token budget — byte-locked to claude-code
/// `DEFAULT_MAX_OUTPUT_TOKENS` (`FileReadTool/limits.ts:18`). A full text read
/// whose estimated token count exceeds this errors with
/// [`format_max_tokens_exceeded`].
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 25_000;

/// LingXi env tier for Claude-compatible per-call file-reading limits.
pub const MAX_OUTPUT_TOKENS_ENV: &str = "LINGXI_FILE_READ_MAX_OUTPUT_TOKENS";

const STREAMING_READ_CHUNK_BYTES: usize = 64 * 1024;

/// Effective per-call token budget. Invalid, missing, or zero env values preserve
/// the byte-locked default.
fn effective_max_output_tokens() -> u64 {
    std::env::var(MAX_OUTPUT_TOKENS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

/// Tool name byte-lock — matches claude-code tool registry.
pub const TOOL_NAME: &str = "Read";

/// True for the FileRead image extensions (png/jpg/jpeg/gif/webp). Defined here
/// (not via `image_read`) so it compiles when `image-read` is off.
fn is_image_path(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

/// `.pdf` (compiles with `pdf-read` off). Routes to the document path. Defined
/// here (not via `pdf_read`) so the gate / routing logic compiles in both
/// feature states; `pdf_read::is_pdf_path` is the same predicate.
fn is_pdf_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
        == Some("pdf")
}

/// Bytes-per-token ratio for the rough local token estimate, keyed by file
/// extension — 1:1 with `bytesPerTokenForFileType` (`tokenEstimation.ts:215-224`).
/// Dense JSON has many single-char tokens, so its real ratio is ~2 not 4.
#[must_use]
fn bytes_per_token_for_file_type(ext: Option<&str>) -> u64 {
    match ext {
        Some("json" | "jsonl" | "jsonc") => 2,
        _ => 4,
    }
}

/// `roughTokenCountEstimationForFileType(content, ext)` — `Math.round(len /
/// bytesPerToken)` (`tokenEstimation.ts:203-242`). Round-half-up over a
/// nonnegative byte length is `(len + bpt/2) / bpt`. Uses the UTF-8 byte length
/// (TS uses the UTF-16 string `.length`); identical for ASCII, the same
/// convention the microcompact / mcp-output ports already use.
#[must_use]
fn rough_token_count_estimation_for_file_type(content: &str, ext: Option<&str>) -> u64 {
    let bpt = bytes_per_token_for_file_type(ext);
    u64::try_from(content.len())
        .unwrap_or(u64::MAX)
        .saturating_add(bpt / 2)
        / bpt
}

/// Build the byte-locked max-tokens-exceeded error — 1:1 with
/// `MaxFileReadTokenExceededError` (`FileReadTool.ts:175-185`).
#[must_use]
pub fn format_max_tokens_exceeded(token_count: u64, max_tokens: u64) -> String {
    format!(
        "File content ({token_count} tokens) exceeds maximum allowed tokens ({max_tokens}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file."
    )
}

/// Local-estimate token gate — `validateContentTokens(content, ext, maxTokens)`
/// (`FileReadTool.ts:755-772`). TS computes `tokenEstimate =
/// roughTokenCountEstimationForFileType(content, ext)`; early-passes when
/// `!tokenEstimate || tokenEstimate <= maxTokens/4`; otherwise refines with the
/// count_tokens API as `effectiveCount = apiCount ?? tokenEstimate` and errors
/// when `effectiveCount > maxTokens`.
///
/// The count_tokens API client is not reachable from the tool context, so this
/// ports TS's own offline fallback exactly: `apiCount` is `None`, hence
/// `effectiveCount == tokenEstimate`, and the gate errors iff
/// `tokenEstimate > maxTokens`. The `maxTokens/4` early-pass band still admits
/// reads in `(maxTokens/4, maxTokens]` (TS would API-refine them, but its
/// `?? tokenEstimate` fallback also admits them when the API is unavailable),
/// so behavior is identical to claude-code running offline / on a provider
/// without count_tokens. Returns the byte-locked error string on overflow.
fn validate_content_tokens(
    content: &str,
    ext: Option<&str>,
    max_tokens: u64,
) -> Result<(), String> {
    let token_estimate = rough_token_count_estimation_for_file_type(content, ext);
    if token_estimate == 0 || token_estimate <= max_tokens / 4 {
        return Ok(());
    }
    // No reachable count_tokens API → `effectiveCount = tokenEstimate`.
    if token_estimate > max_tokens {
        return Err(format_max_tokens_exceeded(token_estimate, max_tokens));
    }
    Ok(())
}

/// The `Truncated: PARTIAL view` note prefix — byte-locked to claude-code `oIt`
/// (`FileReadTool.ts`): the literal `"[Truncated: PARTIAL view "` followed by an
/// em-dash (U+2014) and a trailing space. The two branch tails below are
/// appended to this to form the full note.
const PARTIAL_VIEW_PREFIX: &str = "[Truncated: PARTIAL view \u{2014} ";

/// Result of a graceful token-budget truncation of a full read.
struct GracefulTruncation {
    /// The truncated content (`U` / `T` in claude-code).
    content: String,
    /// The model-facing line count for the truncated content (`S`).
    line_count: u64,
    /// The `[Truncated: PARTIAL view — …]` banner (`R`). NOT appended to the
    /// model-facing output — see FT-08 at the delivery site: it rides its own
    /// `isMeta` message in `new_messages`, the port of the oracle's
    /// `read_truncation_notice` attachment.
    note: String,
}

/// Gracefully shrink an over-budget FULL read to fit `max_tokens`, mirroring
/// claude-code's post-`validateContentTokens` catch block (`FileReadTool.ts`,
/// the `if(L instanceof Fae && k)` branch). Ported 1:1:
///
/// - `N = max(0.5, contentLen / tokenCount)` — chars-per-token derived from the
///   full-content ratio; `O(s) = s.len() / N` is the per-substring token recount.
/// - Line-based shrink: start at `$ = max(1, min(numLines, floor(numLines *
///   cap / tokenCount * 0.85)))`, then up to 6 iterations of `$ *= 0.7` until
///   the joined head fits (`O(U) <= cap`) or `$ <= 1`.
/// - Char-based fallback (very long lines): if the line shrink still overflows
///   or yields blank, slice raw chars `V = max(1, floor(cap * N * 0.85))`, up to
///   6 iterations of `V *= 0.7`; guard against splitting a UTF-16 surrogate
///   (here: never split inside a Rust `char`, which is the same intent).
///
/// `content_len`/substring lengths use UTF-8 byte length to match LingXi's token
/// model (`rough_token_count_estimation_for_file_type` divides byte length), so
/// `O(U)` is consistent with the estimate that produced `token_count`.
fn truncate_to_token_budget(
    content: &str,
    token_count: u64,
    max_tokens: u64,
    total_lines: u64,
    full_file_path: &str,
) -> GracefulTruncation {
    let cap = max_tokens as f64;
    let token_count_f = token_count.max(1) as f64;
    let content_len = content.len() as f64;
    // `N = Math.max(0.5, f.length / Math.max(1, tokenCount))`.
    let n_ratio = (content_len / token_count_f).max(0.5);
    // `O(V) = V.length / N` — token estimate for a substring of byte length `len`.
    let est_tokens = |byte_len: usize| (byte_len as f64) / n_ratio;

    // Split into lines for the line-based shrink (`D = f.split("\n")`). This is a
    // plain (non-inclusive) split, matching claude-code's `f.split("\n")`.
    let lines: Vec<&str> = content.split('\n').collect();
    let num_lines = lines.len();

    // `$ = max(1, min(D.length, floor(D.length * l / tokenCount * 0.85)))`.
    let mut line_n: usize = ((num_lines as f64) * cap / token_count_f * 0.85)
        .floor()
        .max(1.0) as usize;
    line_n = line_n.clamp(1, num_lines);
    let mut head = lines[..line_n].join("\n");

    // Up to 6 iterations of `$ *= 0.7` until it fits or `$ <= 1`.
    for _ in 0..6 {
        if est_tokens(head.len()) <= cap || line_n <= 1 {
            break;
        }
        line_n = ((line_n as f64) * 0.7).floor().max(1.0) as usize;
        head = lines[..line_n].join("\n");
    }

    let mut char_based = false;
    // Char-based fallback when the line shrink couldn't fit / produced blank.
    if est_tokens(head.len()) > cap || head.trim().is_empty() {
        // `V = max(1, floor(l * N * 0.85))` byte budget.
        let mut byte_v: usize = (cap * n_ratio * 0.85).floor().max(1.0) as usize;
        for _ in 0..6 {
            head = slice_bytes_on_char_boundary(content, byte_v);
            if est_tokens(head.len()) <= cap {
                break;
            }
            byte_v = ((byte_v as f64) * 0.7).floor().max(1.0) as usize;
        }
        char_based = true;
    }

    // `T = U, S = W ? Uu(U,"\n")+1 : $`.
    let line_count = if char_based {
        head.matches('\n').count() as u64 + 1
    } else {
        line_n as u64
    };

    // Note branch: line-paging when `!W && S < h`, else the long-lines/char note.
    //
    // FT-08: BOTH branches open with `${r}: `, where `r` is the resolved
    // fullFilePath — `R=…gmt+`${r}: showing lines 1-…`` (cc-238.js @294616079).
    // LingXi dropped that segment; `full_file_path` restores it.
    let note = if !char_based && line_count < total_lines {
        format!(
            "{PARTIAL_VIEW_PREFIX}{full_file_path}: showing lines 1-{line_count} of {total_lines} total ({token_count} tokens, cap {max_tokens}). Call {read_name} with offset={next} limit={line_count} for the next page, or {grep_name} to find a specific section. Do NOT answer from this page alone if the answer may be further in the file.]",
            read_name = TOOL_NAME,
            grep_name = GREP_TOOL_NAME,
            next = line_count + 1,
        )
    } else {
        format!(
            "{PARTIAL_VIEW_PREFIX}{full_file_path}: showing the first {shown} of {full} characters ({token_count} tokens, cap {max_tokens}); this file has very long lines and cannot be paginated by line. Use {grep_name} to find a specific section, or {read_name} with offset/limit to page through it. Do NOT answer from this excerpt alone if the answer may be elsewhere in the file.]",
            shown = head.len(),
            full = content.len(),
            grep_name = GREP_TOOL_NAME,
            read_name = TOOL_NAME,
        )
    };

    GracefulTruncation {
        content: head,
        line_count,
        note,
    }
}

/// Slice `content` to at most `byte_budget` bytes, backing off to the previous
/// UTF-8 character boundary — the safe analogue of claude-code's surrogate-pair
/// guard (`if(Q>=55296&&Q<=56319) U=U.slice(0,-1)`), which avoids splitting a
/// multi-unit character. Rust strings are UTF-8, so we simply refuse to slice
/// mid-`char`.
fn slice_bytes_on_char_boundary(content: &str, byte_budget: usize) -> String {
    let mut end = byte_budget.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    content[..end].to_string()
}

/// The Grep tool name byte-lock — claude-code `$c` resolves to this in the
/// `Truncated: PARTIAL view` note interpolations.
const GREP_TOOL_NAME: &str = "Grep";

/// Human-readable file size, byte-faithful to claude-code `formatFileSize`
/// (`src/utils/format.ts:9-23`): `< 1KB` ⇒ `"{n} bytes"`; otherwise one decimal
/// with a trailing `.0` trimmed, suffixed `KB`/`MB`/`GB` (no space). Used by the
/// too-large read message ([`format_too_large`]) and the PDF routing/extraction
/// messages.
// The u64 → f64 cast loses precision for values > 2^53 (> 9 PB). File sizes
// of that magnitude are not realistic, and claude-code uses JavaScript number
// (f64) for the same computation. The cast is intentional. Always compiled (no
// feature gate) — `format_too_large` is a core text-read message reachable in
// every feature combination.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn format_file_size(size_in_bytes: u64) -> String {
    fn trim(x: f64) -> String {
        let s = format!("{x:.1}");
        s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
    }
    let kb = size_in_bytes as f64 / 1024.0;
    if kb < 1.0 {
        return format!("{size_in_bytes} bytes");
    }
    if kb < 1024.0 {
        return format!("{}KB", trim(kb));
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{}MB", trim(mb));
    }
    let gb = mb / 1024.0;
    format!("{}GB", trim(gb))
}

struct StreamedTextRange {
    content: String,
    total_lines: u64,
    read_lines: u64,
}

fn format_line_too_long(path: &Path, line: u64) -> String {
    format!(
        "File {} contains a line longer than {} at or after requested line {line}. Use Grep to find a smaller section, or use a registered shell tool that can stream the file.",
        path.display(),
        format_file_size(MAX_FILE_READ_SIZE),
    )
}

async fn read_text_range_streaming<R: Read>(
    mut reader: R,
    canon: &Path,
    offset: u64,
    limit: u64,
    ext: Option<&str>,
    max_output_tokens: u64,
) -> Result<StreamedTextRange, ToolError> {
    let end_line = offset.saturating_add(limit);
    let max_line_bytes = MAX_FILE_READ_SIZE as usize;
    let bytes_per_token = bytes_per_token_for_file_type(ext);
    let max_selected_bytes = usize::try_from(max_output_tokens.saturating_mul(bytes_per_token))
        .unwrap_or(usize::MAX)
        .max(max_line_bytes.saturating_add(1));

    let mut selected = Vec::new();
    let mut current_line = 1_u64;
    let mut selected_line_bytes = 0_usize;
    let mut read_lines = 0_u64;
    let mut newline_count = 0_u64;
    let mut total_bytes_read = 0_usize;
    let mut nul_scan_remaining = NUL_SCAN_WINDOW;
    let mut utf8_tail: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; STREAMING_READ_CHUNK_BYTES];

    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|error| ToolError::Io(error.to_string()))?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        total_bytes_read += n;

        let scan = nul_scan_remaining.min(n);
        if scan > 0 {
            if looks_binary(&chunk[..scan]) {
                return Err(ToolError::Io(format_binary(canon)));
            }
            nul_scan_remaining -= scan;
        }

        validate_utf8_stream_chunk(&mut utf8_tail, chunk)
            .map_err(|()| ToolError::Io(format!("File {} is not valid UTF-8", canon.display())))?;

        for &byte in chunk {
            let selecting = current_line >= offset && current_line < end_line;
            if selecting {
                selected.push(byte);
                selected_line_bytes += 1;
                if selected_line_bytes > max_line_bytes {
                    return Err(ToolError::Io(format_line_too_long(canon, current_line)));
                }
                if selected.len() > max_selected_bytes {
                    let estimate = u64::try_from(selected.len())
                        .unwrap_or(u64::MAX)
                        .saturating_add(bytes_per_token / 2)
                        / bytes_per_token;
                    return Err(ToolError::Io(format_max_tokens_exceeded(
                        estimate,
                        max_output_tokens,
                    )));
                }
            }
            if byte == b'\n' {
                newline_count += 1;
                if selecting {
                    read_lines += 1;
                }
                current_line += 1;
                selected_line_bytes = 0;
            }
        }
    }

    if !utf8_tail.is_empty() {
        return Err(ToolError::Io(format!(
            "File {} is not valid UTF-8",
            canon.display()
        )));
    }
    if selected_line_bytes > 0 && current_line >= offset && current_line < end_line {
        read_lines += 1;
    }

    let content = String::from_utf8(selected)
        .map_err(|_| ToolError::Io(format!("File {} is not valid UTF-8", canon.display())))?;
    let total_lines = if total_bytes_read == 0 {
        0
    } else {
        newline_count + 1
    };
    Ok(StreamedTextRange {
        content,
        total_lines,
        read_lines,
    })
}

fn validate_utf8_stream_chunk(tail: &mut Vec<u8>, chunk: &[u8]) -> Result<(), ()> {
    tail.extend_from_slice(chunk);
    match std::str::from_utf8(tail) {
        Ok(_) => {
            tail.clear();
            Ok(())
        }
        Err(e) if e.error_len().is_none() => {
            let valid = e.valid_up_to();
            let suffix = tail[valid..].to_vec();
            if suffix.len() > 4 {
                return Err(());
            }
            *tail = suffix;
            Ok(())
        }
        Err(_) => Err(()),
    }
}

/// Turn rendered page JPEGs into (image sources, tool-result data). Each page is
/// run through [`crate::image_read::process_image`] (the claude-code image
/// ladder) and becomes one [`lingxi_core::types::ImageSource::Base64`]; the tool-result
/// text mirrors claude-code `FileReadTool.ts:684`.
///
/// # Errors
/// Propagates `process_image` failure (a page that fails to decode/encode).
#[cfg(feature = "pdf-render")]
pub(crate) fn build_pages_payload(
    canon: &std::path::Path,
    original_size: u64,
    page_jpegs: Vec<Vec<u8>>,
) -> Result<
    (
        Vec<lingxi_core::types::ImageSource>,
        serde_json::Value,
        String,
    ),
    String,
> {
    let count = page_jpegs.len();
    let mut sources = Vec::with_capacity(count);
    for jpeg in page_jpegs {
        let processed = crate::image_read::process_image(jpeg)?;
        sources.push(lingxi_core::types::ImageSource::Base64 {
            media_type: processed.media_type,
            data: processed.base64,
        });
    }
    let path_str = canon.display().to_string();
    let model_content = format!(
        "PDF pages extracted: {count} page(s) from {path_str} ({})",
        format_file_size(original_size)
    );
    // binary `parts` is file-splitting (`file:{filePath, originalSize, outputDir,
    // count}`); LingXi reuses `type:"parts"` for PDF→page-images (no outputDir),
    // so `file` carries the available metadata + the render rides on model_content.
    let data = serde_json::json!({
        "type": "parts",
        "file": {
            "filePath": path_str,
            "originalSize": original_size,
            "count": count,
        },
    });
    Ok((sources, data, model_content))
}

/// Build the too-large error message — byte-locked VERBATIM to claude-code
/// `FileTooLargeError` (`utils/readFileInRange.ts:62-64`). Both sizes are
/// rendered with [`format_file_size`] (TS `formatFileSize`). The `path` argument
/// is unused by the TS message (it interpolates only the two sizes); kept in the
/// signature so existing call sites need no change.
#[must_use]
pub fn format_too_large(_path: &std::path::Path, size: u64) -> String {
    format!(
        "File content ({}) exceeds maximum allowed size ({}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.",
        format_file_size(size),
        format_file_size(MAX_FILE_READ_SIZE),
    )
}

/// Binary file extensions (with leading dot, lowercase) — 1:1 port of
/// claude-code `BINARY_EXTENSIONS` (`constants/files.ts:5-112`). A file whose
/// extension is in this set is rejected as binary *before* any read, EXCEPT the
/// extensions the tool renders natively (the 5 image exts + `.pdf`), which route
/// to the image / PDF paths earlier (`FileReadTool.ts:472-476`: `hasBinaryExtension
/// && !isPDFExtension && !IMAGE_EXTENSIONS.has(...)`).
static BINARY_EXTENSIONS: Lazy<std::collections::HashSet<&'static str>> = Lazy::new(|| {
    [
        // Images
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".webp", ".tiff", ".tif",
        // Videos
        ".mp4", ".mov", ".avi", ".mkv", ".webm", ".wmv", ".flv", ".m4v", ".mpeg", ".mpg",
        // Audio
        ".mp3", ".wav", ".ogg", ".flac", ".aac", ".m4a", ".wma", ".aiff", ".opus",
        // Archives
        ".zip", ".tar", ".gz", ".bz2", ".7z", ".rar", ".xz", ".z", ".tgz", ".iso",
        // Executables/binaries
        ".exe", ".dll", ".so", ".dylib", ".bin", ".o", ".a", ".obj", ".lib", ".app", ".msi", ".deb",
        ".rpm",
        // Documents (PDF is here; the call site excludes it — rendered natively)
        ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".ods", ".odp",
        // Fonts
        ".ttf", ".otf", ".woff", ".woff2", ".eot", // Bytecode / VM artifacts
        ".pyc", ".pyo", ".class", ".jar", ".war", ".ear", ".node", ".wasm", ".rlib",
        // Database files
        ".sqlite", ".sqlite3", ".db", ".mdb", ".idx", // Design / 3D
        ".psd", ".ai", ".eps", ".sketch", ".fig", ".xd", ".blend", ".3ds", ".max",
        // Flash
        ".swf", ".fla", // Lock/profiling data
        ".lockb", ".dat", ".data",
    ]
    .into_iter()
    .collect()
});

/// `hasBinaryExtension(filePath)` — 1:1 with `constants/files.ts:117-120`. Takes
/// the substring from the last `.` (inclusive), lowercases it, and checks
/// membership in [`BINARY_EXTENSIONS`]. A path with no `.` yields the whole path
/// lowercased (TS `slice(lastIndexOf('.'))` returns the full string when there is
/// no dot), which will not be in the set.
#[must_use]
fn has_binary_extension(path: &std::path::Path) -> bool {
    let name = path.to_string_lossy();
    let ext = match name.rfind('.') {
        Some(i) => name[i..].to_ascii_lowercase(),
        None => name.to_ascii_lowercase(),
    };
    BINARY_EXTENSIONS.contains(ext.as_str())
}

/// Build the binary-file error message — byte-locked VERBATIM to claude-code
/// `FileReadTool.ts:479`. TS renders the file's lowercased extension (e.g.
/// `.bin`) into the message; [`format_binary`] derives it from `path` the same
/// way (`path.extname(...).toLowerCase()`), falling back to an empty extension
/// for extensionless files (matching TS `path.extname` returning `""`). Used by
/// BOTH the extension gate (`has_binary_extension`) and the NUL-scan fallback (an
/// extensionless or non-listed binary file whose first 8 KB contain a NUL byte).
#[must_use]
pub fn format_binary(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(String::new, |e| format!(".{}", e.to_ascii_lowercase()));
    format!(
        "This tool cannot read binary files. The file appears to be a binary {ext} file. Please use appropriate tools for binary file analysis."
    )
}

/// Whether `path` is a blocking device/special file that Read must refuse —
/// 1:1 with claude-code `$3p` (#11): membership in the fixed `/dev` set `U3p`,
/// any `/proc/…/fd/{0,1,2}`, or `^/proc/[^/]+/(environ|cmdline|auxv|maps|mem|
/// stat)$`. Reading these would block or produce infinite output.
#[must_use]
pub fn is_device_file(path: &str) -> bool {
    const DEVICE_PATHS: &[&str] = &[
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/full",
        "/dev/stdin",
        "/dev/tty",
        "/dev/console",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd/0",
        "/dev/fd/1",
        "/dev/fd/2",
    ];
    if DEVICE_PATHS.contains(&path) {
        return true;
    }
    if path.starts_with("/proc/")
        && (path.ends_with("/fd/0") || path.ends_with("/fd/1") || path.ends_with("/fd/2"))
    {
        return true;
    }
    // `^/proc/[^/]+/(environ|cmdline|auxv|maps|mem|stat)$` — exactly `/proc/`,
    // one non-empty non-slash segment, `/`, then one of the six names, end.
    if let Some(rest) = path.strip_prefix("/proc/") {
        let mut parts = rest.split('/');
        if let (Some(seg), Some(name), None) = (parts.next(), parts.next(), parts.next()) {
            if !seg.is_empty()
                && matches!(
                    name,
                    "environ" | "cmdline" | "auxv" | "maps" | "mem" | "stat"
                )
            {
                return true;
            }
        }
    }
    false
}

/// Narrow no-break space (U+202F) used by some macOS versions in screenshot
/// filenames before AM/PM — byte-locked to claude-code `THIN_SPACE`
/// (`FileReadTool.ts:131`, `String.fromCharCode(8239)`).
const THIN_SPACE: char = '\u{202F}';

/// `getAlternateScreenshotPath(filePath)` — 1:1 with `FileReadTool.ts:147-159`.
/// macOS screenshot filenames put either a regular space (`' '`) or a thin space
/// (U+202F) before `AM`/`PM` depending on the OS version; a model often passes
/// the wrong one. When the basename matches `^(.+)([  ])(AM|PM)(\.png)$`,
/// returns the path with the alternate space character so the caller can retry
/// the read before giving up. Returns `None` when the basename does not match.
///
/// The TS uses a regex on `path.basename`; this mirrors it with a manual scan
/// (no regex dep): require a `.png` suffix, then an `AM`/`PM` immediately before
/// it, then a single space-or-thin-space immediately before that, with at least
/// one char ahead of the space (TS `(.+)`). The space is swapped only in the
/// trailing `"{space}{AM|PM}.png"` occurrence (TS `String.replace` replaces the
/// FIRST match — but the constructed needle is the unique tail, so first == the
/// tail in practice; we replace that exact tail).
#[must_use]
fn get_alternate_screenshot_path(file_path: &std::path::Path) -> Option<PathBuf> {
    let filename = file_path.file_name()?.to_str()?;
    // (.+)([  ])(AM|PM)(\.png)$
    let stem = filename.strip_suffix(".png")?;
    let (head, am_pm) = if let Some(h) = stem.strip_suffix("AM") {
        (h, "AM")
    } else if let Some(h) = stem.strip_suffix("PM") {
        (h, "PM")
    } else {
        return None;
    };
    let mut chars = head.chars();
    let current_space = chars.next_back()?;
    if current_space != ' ' && current_space != THIN_SPACE {
        return None;
    }
    // TS `(.+)` requires at least one character before the space.
    if chars.as_str().is_empty() {
        return None;
    }
    let alternate_space = if current_space == ' ' {
        THIN_SPACE
    } else {
        ' '
    };
    // TS replaces `${currentSpace}${AM|PM}${.png}` with the alternate-space
    // form in the FULL path string. Build the same needle/replacement and apply
    // it to the path's string form.
    let needle = format!("{current_space}{am_pm}.png");
    let replacement = format!("{alternate_space}{am_pm}.png");
    let path_str = file_path.to_str()?;
    Some(PathBuf::from(path_str.replacen(&needle, &replacement, 1)))
}

/// `findSimilarFile(filePath)` — 1:1 with `utils/file.ts:178-207`. When a read
/// targets a missing path, scan the target's PARENT directory for the FIRST
/// file whose base name WITHOUT its extension equals the target's base name
/// without its extension, excluding the target itself, and return that file's
/// name (with extension). The heuristic is purely "same stem, different
/// extension" (e.g. `App.tsx` ⇒ `App.ts`) — NOT edit-distance or prefix
/// matching. Returns `None` when the directory is unreadable (TS catches
/// ENOENT/other and returns `undefined`) or no sibling shares the stem.
///
/// `file_stem`/`extension` here mirror TS `basename(p, extname(p))` /
/// `extname(file.name)`: the part before the LAST dot. (Rust `Path::file_stem`
/// matches `basename(p, extname(p))` for the common cases; for dotfiles like
/// `.env` Rust's stem is `.env` while TS `basename('.env', extname('.env'))`
/// is also `.env` since `extname('.env') === ''` — they agree.)
#[must_use]
pub(crate) fn find_similar_file(file_path: &std::path::Path) -> Option<String> {
    let dir = file_path.parent()?;
    let target_stem = file_path.file_stem()?;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let entry_path = dir.join(&name);
        // Same base name without extension, and not the target file itself.
        if std::path::Path::new(&name).file_stem() == Some(target_stem) && entry_path != file_path {
            return Some(name.to_string_lossy().into_owned());
        }
    }
    None
}

/// Marker included in file-not-found error messages that carry a cwd note —
/// byte-locked to claude-code `FILE_NOT_FOUND_CWD_NOTE` (`utils/file.ts:213`).
/// claude-code's UI renderers check for this prefix to show a short
/// "File not found" message; the port keeps it verbatim so the model-facing
/// string matches.
pub(crate) const FILE_NOT_FOUND_CWD_NOTE: &str = "Note: your current working directory is";

/// Build the byte-locked file-not-found message shared by Read (`FileReadTool`
/// ENOENT arm) and Edit (`FileEditTool.validateInput`, errorCode 4). Both sites
/// are the SAME three lines in claude-code 2.1.238:
///
/// ```text
/// let similar = await findSimilarFile(p), corrected = await suggestPathUnderCwd(p)
/// let message = `File does not exist. ${FILE_NOT_FOUND_CWD_NOTE} ${getCwd()}.`
/// if (corrected)    message += ` Did you mean ${corrected}?`
/// else if (similar) message += ` Did you mean ${similar}?`
/// ```
///
/// `live_cwd` is the caller's live cwd (`Ct()`); it is realpath-resolved here
/// (falling back to the unresolved path) so the rendered cwd AND the
/// [`suggest_path_under_cwd`] prefix comparison both use the same resolved form.
#[must_use]
pub(crate) fn file_not_found_message(
    canon: &std::path::Path,
    live_cwd: &std::path::Path,
) -> String {
    let cwd = std::fs::canonicalize(live_cwd).unwrap_or_else(|_| live_cwd.to_path_buf());
    let mut message = format!(
        "File does not exist. {FILE_NOT_FOUND_CWD_NOTE} {}.",
        cwd.display()
    );
    // The cwd "dropped repo folder" suggestion takes PRECEDENCE over the
    // same-stem sibling. Both suffixes are the VERBATIM `" Did you mean {x}?"`.
    if let Some(cwd_suggestion) = suggest_path_under_cwd(canon, &cwd) {
        message.push_str(&format!(" Did you mean {cwd_suggestion}?"));
    } else if let Some(similar) = find_similar_file(canon) {
        message.push_str(&format!(" Did you mean {similar}?"));
    }
    message
}

/// `suggestPathUnderCwd(requestedPath)` — 1:1 with `utils/file.ts:228-267`.
/// Detects the "dropped repo folder" pattern: the model builds an absolute path
/// that is missing the repo-directory component (e.g. `/Users/x/src/foo` when
/// cwd is `/Users/x/src/currentRepo`), and the SAME relative path exists under
/// cwd (`/Users/x/src/currentRepo/foo`). When so, returns that corrected path so
/// the not-found message can suggest it; otherwise `None`.
///
/// The port mirrors TS exactly:
///   1. `cwdParent = dirname(cwd)` (the caller passes the realpath-resolved cwd,
///      mirroring TS `getCwd()` which is already realpath-resolved).
///   2. `resolvedPath = realpath(dirname(requestedPath))` joined with
///      `basename(requestedPath)`; on a realpath error, the original
///      `requestedPath` is used as-is (TS `try/catch`). Resolving the requested
///      path's PARENT (`std::fs::canonicalize`, the `realpath` analog) makes the
///      symlink-resolved prefix comparison line up with the already-resolved cwd
///      (e.g. `/tmp` → `/private/tmp` on macOS).
///   3. `cwdParentPrefix = (cwdParent === sep) ? sep : cwdParent + sep` — the
///      root-directory case avoids a never-matching `//` double separator.
///   4. Suggest ONLY when `resolvedPath` starts with `cwdParentPrefix` AND is
///      neither under `cwd + sep` nor equal to `cwd` (i.e. under cwd's PARENT but
///      not under cwd itself).
///   5. `relFromParent = relative(cwdParent, resolvedPath)`,
///      `correctedPath = join(cwd, relFromParent)`.
///   6. Return `Some(correctedPath)` iff `stat(correctedPath)` succeeds
///      (`std::fs::metadata(...).is_ok()`), else `None`.
///
/// The `startsWith` / equality checks mirror TS's string comparisons over the
/// path strings (using [`std::path::MAIN_SEPARATOR`] for `sep`); the `relative` /
/// `join` use `Path` operations. Because step 4 already proved `resolvedPath` is
/// strictly under `cwdParent`, the `relative` is exactly the suffix after
/// `cwdParent`, which [`std::path::Path::strip_prefix`] yields.
#[must_use]
pub(crate) fn suggest_path_under_cwd(
    requested: &std::path::Path,
    cwd: &std::path::Path,
) -> Option<String> {
    use std::path::MAIN_SEPARATOR;

    // `cwdParent = dirname(cwd)`. A cwd with no parent (the filesystem root) has
    // no enclosing parent to look under → no suggestion.
    let cwd_parent = cwd.parent()?;

    // `resolvedPath = realpath(dirname(requestedPath))` joined with the basename,
    // falling back to the raw requested path when the parent can't be resolved
    // (TS `try { realpath } catch {}`).
    let file_name = requested.file_name()?;
    let resolved_path: PathBuf = match requested.parent() {
        Some(parent) => match std::fs::canonicalize(parent) {
            Ok(resolved_dir) => resolved_dir.join(file_name),
            // Parent directory doesn't exist — use the original path.
            Err(_) => requested.to_path_buf(),
        },
        None => requested.to_path_buf(),
    };

    // String forms for the TS `startsWith` / equality semantics.
    let resolved_str = resolved_path.to_string_lossy();
    let cwd_str = cwd.to_string_lossy();
    let cwd_parent_str = cwd_parent.to_string_lossy();

    // `cwdParentPrefix = cwdParent === sep ? sep : cwdParent + sep`.
    let cwd_parent_prefix = if cwd_parent_str.as_ref() == MAIN_SEPARATOR.to_string() {
        MAIN_SEPARATOR.to_string()
    } else {
        format!("{cwd_parent_str}{MAIN_SEPARATOR}")
    };
    let cwd_prefix = format!("{cwd_str}{MAIN_SEPARATOR}");

    // Only suggest when the requested path is under cwd's PARENT but not under
    // cwd itself (TS: `!startsWith(cwdParentPrefix) || startsWith(cwd+sep) ||
    // === cwd` ⇒ undefined).
    if !resolved_str.starts_with(&cwd_parent_prefix)
        || resolved_str.starts_with(&cwd_prefix)
        || resolved_str == cwd_str
    {
        return None;
    }

    // `relFromParent = relative(cwdParent, resolvedPath)` — the suffix after
    // `cwdParent` (guaranteed strict-prefix by the check above);
    // `correctedPath = join(cwd, relFromParent)`.
    let rel_from_parent = resolved_path.strip_prefix(cwd_parent).ok()?;
    let corrected_path = cwd.join(rel_from_parent);

    // `try { await stat(correctedPath); return correctedPath } catch { undefined }`.
    if std::fs::metadata(&corrected_path).is_ok() {
        Some(corrected_path.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// `MAX_FILE_EXTENSION_LENGTH` — byte-locked to claude-code
/// (`services/analytics/metadata.ts:311`). Extensions longer than this bucket to
/// `"other"` so analytics never leaks a long, potentially-identifying suffix.
const MAX_FILE_EXTENSION_LENGTH: usize = 10;

/// `getFileExtensionForAnalytics(filePath)` — 1:1 with
/// `services/analytics/metadata.ts:323-337`. Returns the lowercased extension
/// without the leading dot, `Some("other")` when it exceeds
/// [`MAX_FILE_EXTENSION_LENGTH`], and `None` when the path has no extension (TS
/// `undefined` for empty / `"."`). The analytics ext is intentionally distinct
/// from the slicing `ext` used elsewhere: TS sources the `tengu_*` `ext` metadata
/// from this helper (the `AnalyticsMetadata_I_VERIFIED_THIS_IS_NOT_CODE_OR_FILEPATHS`
/// type), not from the raw `path.extname` used for token estimation.
#[must_use]
fn get_file_extension_for_analytics(path: &std::path::Path) -> Option<String> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    if ext.is_empty() {
        return None;
    }
    let lower = ext.to_ascii_lowercase();
    if lower.len() > MAX_FILE_EXTENSION_LENGTH {
        return Some("other".to_string());
    }
    Some(lower)
}

/// `detectSessionFileType(filePath)` — 1:1 with
/// `utils/memoryFileDetection.ts:40-59`. Returns `("session_memory")` when the
/// path is under `<configHome>/.../session-memory/*.md`, `("session_transcript")`
/// when under `<configHome>/.../projects/*.jsonl`, else `None`. The path is
/// compared in forward-slash form against the resolved config home
/// (`$LINGXI_CONFIG_DIR ?? $HOME/.claude`, mirroring `getClaudeConfigHomeDir()`).
/// On POSIX (the port target) `toComparable` is a no-op beyond separator
/// normalization (no case-folding); Windows case-folding is not reproduced here.
// The `.md` / `.jsonl` suffix checks are a faithful 1:1 port of TS's
// case-SENSITIVE `normalized.endsWith('.md')` / `endsWith('.jsonl')` (TS only
// case-folds on Windows, via `toComparable`, which this POSIX port does not do).
// A case-insensitive comparison would change the matching semantics, so the
// `case_sensitive_file_extension_comparisons` lint is intentionally suppressed.
#[must_use]
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn detect_session_file_type(path: &std::path::Path) -> Option<&'static str> {
    let config_dir = lingxi_config_home_dir();
    let normalized = to_comparable(path);
    let config_cmp = to_comparable(&config_dir);
    if !normalized.starts_with(&config_cmp) {
        return None;
    }
    if normalized.contains("/session-memory/") && normalized.ends_with(".md") {
        return Some("session_memory");
    }
    if normalized.contains("/projects/") && normalized.ends_with(".jsonl") {
        return Some("session_transcript");
    }
    None
}

/// Forward-slash form of a path (`toPosix`/`toComparable` on POSIX —
/// `utils/memoryFileDetection.ts:25-34`). No case-folding (POSIX target).
fn to_comparable(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Port of claude-code `tr()` (`$LINGXI_CONFIG_DIR ?? join(home, ".lingxi")`):
/// `$LINGXI_CONFIG_DIR` when set is honored verbatim (`??`, incl. an empty value
/// → cwd-relative), else `$HOME/.claude` (falling back to `USERPROFILE` then a
/// bare `.claude`). Mirrors the ports in `tools/task` / `commands/core`.
fn lingxi_config_home_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return PathBuf::from(dir);
    }
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(home) => PathBuf::from(home).join(branding::DOT_DIR),
        None => PathBuf::from(branding::DOT_DIR),
    }
}

// REMOVED (parity verdict 12/14): the per-file-read cyber-risk/malware
// `<system-reminder>` does NOT exist in claude-code v2.1.183 (grep counts for
// "considered malware" / "shouldIncludeFileReadMitigation" / "refuse to improve"
// are all 0 in the binary). The text-read result mapper there appends ONLY the
// token-cap partial-view reminder + memory-age reminder + cat -n line numbers.
// The defensive-security / malware guidance now lives in the SYSTEM-PROMPT BODY
// (the `zHo` literal inside `Pym`, ported to
// `orchestrator::prompt::body_sections`) — i.e. in the prompt, not the tool.
// The former `CYBER_RISK_MITIGATION_REMINDER` const, `MITIGATION_EXEMPT_MODELS`,
// and `should_include_file_read_mitigation` were deleted accordingly.

/// Model-facing Read-dedup (`file_unchanged`) LONG form — byte-locked to
/// claude-code `FOg` (2.1.220 binary offset 229141593; `tld` in older builds).
///
/// **DETECT-ONLY in 2.1.220.** `FOg` has NO PRODUCER in the current binary:
/// `LC_ALL=C grep -abo -F 'FOg'` yields exactly two hits in the readable-JS
/// region — its own definition (@229141593) and the detector
/// `joo(e){return e.startsWith(FOg)||e.startsWith(Sou)||e.startsWith(Tou)}`
/// (@229139264, `FOg` reference @229139291). It is retained solely to recognise
/// dedup stubs written into transcripts by OLDER versions. The live producer is
/// [`FILE_UNCHANGED_SHORT`] (`Sou`, @229141759) via `Eou(){return Sou}`
/// (@229139080), selected by the result renderer at @235742950.
pub const FILE_UNCHANGED_STUB: &str = "File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current — refer to that instead of re-reading.";

/// Short-form Read dedup message — byte-locked to claude-code `Sou`
/// (2.1.220 binary offset 229141759), returned by `Eou(){return Sou}`
/// (@229139080). This is the ONLY string the non-seeded `file_unchanged` branch
/// emits in 2.1.220 (renderer @235742950:
/// `content: e.source==="seeded" ? vou(...) : Eou()`). The em-dash is U+2014,
/// matching the binary literal exactly.
pub const FILE_UNCHANGED_SHORT: &str =
    "Wasted call \u{2014} file unchanged since your last Read. Refer to that earlier tool_result instead.";

/// `Tou` (2.1.220 binary offset 229141864) — the constant PREFIX of the SEEDED
/// dedup stub built by
/// ``vou(e){return `${Tou} (see "Contents of ${e}" above) and has not changed on
/// disk. Use that content instead of re-reading.</system-reminder>`}`` (@229139106).
///
/// PRODUCED by the seeded Read-dedup branch via
/// [`format_file_unchanged_seeded`], and matched by [`is_dedup_result`] so the
/// detector stays a 1:1 port of `joo`.
pub const FILE_UNCHANGED_SEEDED_PREFIX: &str =
    "<system-reminder>This file is already in your context";

/// `vou(e)` (2.1.220 @229139106) — the SEEDED `file_unchanged` stub:
///
/// ```text
/// vou(e){return `${Tou} (see "Contents of ${e}" above) and has not changed on
///                disk. Use that content instead of re-reading.</system-reminder>`}
/// ```
///
/// where `Tou` = [`FILE_UNCHANGED_SEEDED_PREFIX`]. The result renderer
/// (@235742950) calls it as `vou(e.file.filePath)`, i.e. with the SAME
/// `fullFilePath` the dedup branch put into `data.file.filePath` — the
/// canonicalized absolute path on the Rust side.
#[must_use]
pub fn format_file_unchanged_seeded(path: &std::path::Path) -> String {
    format!(
        "{FILE_UNCHANGED_SEEDED_PREFIX} (see \"Contents of {}\" above) and has not changed on disk. Use that content instead of re-reading.</system-reminder>",
        path.display()
    )
}

/// `joo(e)` (2.1.220 binary offset 229139264) — byte-locked dedup detector:
/// `e.startsWith(FOg)||e.startsWith(Sou)||e.startsWith(Tou)`, i.e. the legacy
/// long form ([`FILE_UNCHANGED_STUB`]), the live short form
/// ([`FILE_UNCHANGED_SHORT`]) or the seeded-stub prefix
/// ([`FILE_UNCHANGED_SEEDED_PREFIX`]). Used to detect a dedup response in
/// tool-result filtering / context compaction that must not re-expand.
#[must_use]
pub fn is_dedup_result(s: &str) -> bool {
    s.starts_with(FILE_UNCHANGED_STUB)
        || s.starts_with(FILE_UNCHANGED_SHORT)
        || s.starts_with(FILE_UNCHANGED_SEEDED_PREFIX)
}

/// Model-facing warning when a read targets an existing but empty file —
/// byte-locked to claude-code (`FileReadTool.ts:705-706`).
pub const EMPTY_FILE_WARNING: &str =
    "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>";

/// The LONG Read prompt — byte-locked VERBATIM to claude-code `Yhi(e,…)`'s
/// `Dh(e)===false` branch (binary offset ~195602718), resolved for the default
/// build:
///   * `${LQe}` (`MAX_LINES_TO_READ`) => 2000;
///   * `${maxSizeInstruction}` => "" (`khe().includeMaxSizeInPrompt` defaults
///     undefined/false — no `tengu_amber_wren` override);
///   * `${offsetInstruction}` => `Khi` (`khe().targetedRangeNudge` undefined/false);
///   * `${lineFormat}` => `K3p()`=`VBr` (`N$e()`=`tengu_tab_read_sep` defaults
///     false ⇒ the `cat -n` line, not the tab-aware `Vhi` variant);
///   * the `isPDFSupported()?…:''` (`PQe()`) fragment => INCLUDED (the default
///     model is not `claude-3-haiku`);
///   * `${qhi}` final bullet => appended (present in BOTH branches; em-dash is
///     U+2014).
/// The `cat -n` / directory / shell-tool wording matches the binary exactly.
const READ_PROMPT_LONG: &str = "Reads a file from the local filesystem. You can access any file directly by using this tool.\n\
Assume this tool is able to read all files on the machine. If the User provides a path to a file assume that path is valid. It is okay to read a file that does not exist; an error will be returned.\n\
\n\
Usage:\n\
- The file_path parameter must be an absolute path, not a relative path\n\
- By default, it reads up to 2000 lines starting from the beginning of the file\n\
- You can optionally specify a line offset and limit (especially handy for long files), but it's recommended to read the whole file by not providing these parameters\n\
- Results are returned using cat -n format, with line numbers starting at 1\n\
- This tool allows Claude Code to read images (eg PNG, JPG, etc). When reading an image file the contents are presented visually as Claude Code is a multimodal LLM.\n\
- This tool can read PDF files (.pdf). For large PDFs (more than 10 pages), you MUST provide the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). Reading a large PDF without the pages parameter will fail. Maximum 20 pages per request.\n\
- This tool can read Jupyter notebooks (.ipynb files) and returns all cells with their outputs, combining code, text, and visualizations.\n\
- This tool can only read files, not directories. To list files in a directory, use the registered shell tool.\n\
- You will regularly be asked to read screenshots. If the user provides a path to a screenshot, ALWAYS use this tool to view the file at the path. This tool will work with all temporary file paths.\n\
- If you read a file that exists but has empty contents you will receive a system reminder warning in place of file contents.\n\
- Do NOT re-read a file you just edited to verify \u{2014} Edit/Write would have errored if the change failed, and the harness tracks file state for you.";

/// The SHORT Read prompt — byte-locked VERBATIM to claude-code `Yhi(e,…)`'s
/// `Dh(e)===true` branch (binary offset 195602702), served to current-gen
/// default models. Interpolations resolved as in [`READ_PROMPT_LONG`]:
///   * `${LQe}` => 2000;  `${n}` (maxSizeInstruction) => "";
///   * `${r}` (offsetInstruction) => `Khi`; `${t}` (lineFormat) => `VBr`;
///   * `${PQe()?…:""}` PDF fragment => INCLUDED;  `${qhi}` => appended.
/// The ellipsis in "(PNG, JPG, …)" is U+2026; the qhi em-dash is U+2014.
const READ_PROMPT_SHORT: &str = "Reads a file from the local filesystem.\n\
\n\
- `file_path` must be an absolute path.\n\
- Reads up to 2000 lines by default.\n\
- You can optionally specify a line offset and limit (especially handy for long files), but it's recommended to read the whole file by not providing these parameters\n\
- Results are returned using cat -n format, with line numbers starting at 1\n\
- Reads images (PNG, JPG, \u{2026}) and presents them visually. Reads PDFs via the `pages` parameter (e.g. \"1-5\", max 20 pages/request; required for PDFs over 10 pages). Reads Jupyter notebooks (.ipynb) as cells with outputs.\n\
- Reading a directory, a missing file, or an empty file returns an error or system reminder rather than content.\n\
- Do NOT re-read a file you just edited to verify \u{2014} Edit/Write would have errored if the change failed, and the harness tracks file state for you.";

/// Build the model-facing offset-beyond-EOF warning — byte-locked to claude-code
/// (`FileReadTool.ts:707`). `offset` is the requested 1-based start line
/// (`data.file.startLine`); `total_lines` is TS `readFileInRange.totalLines`
/// (newline count + 1; see the FILE.4 note at the call site).
#[must_use]
pub fn format_offset_beyond_eof(offset: u64, total_lines: u64) -> String {
    format!(
        "<system-reminder>Warning: the file exists but is shorter than the provided offset ({offset}). The file has {total_lines} lines.</system-reminder>"
    )
}

/// `cat -n` line numbering for the model-facing read output — 1:1 with
/// claude-code's compact-format `addLineNumbers` (`utils/file.ts:290-319`,
/// killswitch off = the current default). Each line becomes `{n}\t{line}` where
/// `n` counts up from `start_line` (1-based); lines are joined by `\n`. Empty
/// content yields `""`. Splitting mirrors the TS `/\r?\n/` regex (a trailing
/// `\r` is stripped per line). The legacy padded-arrow format
/// (`String(n).padStart(6, ' ') + "→"`) only applied with the killswitch on and
/// is intentionally not ported.
#[must_use]
pub fn add_line_numbers(content: &str, start_line: u64) -> String {
    if content.is_empty() {
        return String::new();
    }
    content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .zip(start_line..)
        .map(|(line, n)| format!("{n}\t{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `FileReadTool` — reads a UTF-8 file inside the trusted-dirs whitelist.
pub struct FileReadTool {
    ctx: BuiltinToolContext,
    /// Optional shared live-cwd cell (claude-code `getCwd()`/`Ct()`). When
    /// injected (desktop), the "File does not exist. Note: your current working
    /// directory is …" note and its `suggest_path_under_cwd` base follow the
    /// post-`cd` directory; when absent (mobile/tests) they fall back to
    /// `ctx.workspace`.
    live_cwd: Option<tool_api::LiveCwdCell>,
}

fn read_rooted_snapshot(
    requested: &std::path::Path,
    approved: &std::path::Path,
    trusted_dirs: &[std::path::PathBuf],
) -> Result<
    lingxi_core::host::rooted_fs::RootedFileSnapshot,
    lingxi_core::host::rooted_fs::RootedFsError,
> {
    let Some((root, relative)) = crate::shared::rooted_location(approved, trusted_dirs) else {
        return Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
            lingxi_core::host::FsError::OutsideWorkspace(approved.display().to_string()),
        ));
    };
    lingxi_core::host::rooted_fs::read_file_after_permission(&root, &relative, requested, approved)
}

fn open_rooted_file(
    requested: &std::path::Path,
    approved: &std::path::Path,
    trusted_dirs: &[std::path::PathBuf],
) -> Result<std::fs::File, lingxi_core::host::rooted_fs::RootedFsError> {
    let Some((root, relative)) = crate::shared::rooted_location(approved, trusted_dirs) else {
        return Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
            lingxi_core::host::FsError::OutsideWorkspace(approved.display().to_string()),
        ));
    };
    lingxi_core::host::rooted_fs::open_file_after_permission(&root, &relative, requested, approved)
}

fn symlink_resolution_changed_message(path: &str) -> String {
    format!(
        "Refusing to read {path}: its symlink resolution changed after permission was checked. If a link in the working directory is being rewritten concurrently, stop that and retry."
    )
}

impl FileReadTool {
    /// Construct a new tool. Cheap — only clones the shared `Arc`s.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self {
            ctx,
            live_cwd: None,
        }
    }

    /// Inject the shared live-cwd cell (builder; default is `None`). The desktop
    /// composition root passes the SAME cell the `BashTool` writes on a `cd`.
    #[must_use]
    pub fn with_live_cwd(mut self, cell: tool_api::LiveCwdCell) -> Self {
        self.live_cwd = Some(cell);
        self
    }

    /// The effective live cwd: the injected cell's value if present (OURS
    /// P2-08 — tracks a foreground Bash `cd`), else the session cwd
    /// [`BuiltinToolContext::cwd`] (THEIRS worktree-206 — the boot cwd until an
    /// `EnterWorktree`/`ExitWorktree` swaps `session_cwd`). Both are the
    /// claude-code `Ct()` fallback; they coincide at boot with no cell injected.
    fn cwd_now(&self) -> std::path::PathBuf {
        self.live_cwd
            .as_ref()
            .map(|c| c.lock().unwrap().clone())
            .unwrap_or_else(|| self.ctx.cwd())
    }

    async fn emit_started(&self, invocation_id: &str, path: &std::path::Path) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "_PROTO_file_path".to_string(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(path.display().to_string()).into_inner(),
            ),
        );
        self.ctx.bus.log_event(READ_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, bytes_read: u64, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "bytes_read".to_string(),
            AnalyticsValue::Int(bytes_read as i64),
        );
        md.insert(
            "duration_ms".to_string(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(READ_COMPLETED, md).await;
    }

    async fn emit_failed(&self, invocation_id: &str, failure_kind: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "failure_kind".to_string(),
            AnalyticsValue::String(Verified::assert_safe(failure_kind.to_string()).into_inner()),
        );
        self.ctx.bus.log_event(READ_FAILED, md).await;
    }

    /// `tengu_file_read_dedup` (`FileReadTool.ts:559-561`) — fired in the dedup
    /// short-circuit (the `file_unchanged` path). Metadata: `ext` ONLY when
    /// present (TS spreads `...(analyticsExt !== undefined && { ext })`); the
    /// analytics ext comes from [`get_file_extension_for_analytics`].
    ///
    /// `source` is `Some("seeded")` for the SEEDED branch (@235741459:
    /// `{source:Te("seeded"), ...b!==void 0&&{ext:b}}` — BOTH keys) and `None`
    /// for the ordinary Read→Read branch, whose payload is `{...ext}` only.
    async fn emit_file_read_dedup(&self, path: &std::path::Path, source: Option<&'static str>) {
        let mut md: LogEventMetadata = HashMap::new();
        if let Some(source) = source {
            md.insert(
                "source".to_string(),
                AnalyticsValue::String(Verified::assert_safe(source.to_string()).into_inner()),
            );
        }
        if let Some(ext) = get_file_extension_for_analytics(path) {
            md.insert(
                "ext".to_string(),
                AnalyticsValue::String(Verified::assert_safe(ext).into_inner()),
            );
        }
        self.ctx.bus.log_event(FILE_READ_DEDUP, md).await;
    }

    /// `tengu_file_read_reread` (#13) — fired when reading a file that already
    /// has a read-file-state entry, BEFORE the dedup short-circuit. 2.1.220
    /// @235740900:
    /// `if(m)M("tengu_file_read_reread",{priorOp:Te(m.seededFromContext?"seeded"
    /// :m.offset===void 0?"edit_write":"read")})` — a THREE-way with `seeded`
    /// tested FIRST. `prior_op` is `"seeded"` for a memory-seeded entry,
    /// `"read"` for a prior Read-origin entry, `"edit_write"` for a prior
    /// Edit/Write-origin entry.
    async fn emit_file_read_reread(&self, prior_op: &'static str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "priorOp".to_string(),
            AnalyticsValue::String(Verified::assert_safe(prior_op.to_string()).into_inner()),
        );
        self.ctx.bus.log_event(FILE_READ_REREAD, md).await;
    }

    /// `tengu_session_file_read` (`FileReadTool.ts:1069-1083`) — fired after a
    /// successful TEXT read. Metadata mirrors TS exactly:
    /// `totalLines`/`readLines`/`totalBytes`/`readBytes`/`offset` (int, always),
    /// `limit`/`ext`/`messageID` (only-when-present), and the two session-file
    /// booleans. `messageID` is sourced from the assistant message in TS
    /// (`parentMessage`), which has no analog reachable from the Rust tool
    /// context — so it is faithfully OMITTED (matching TS's `undefined` spread).
    #[allow(clippy::too_many_arguments)]
    async fn emit_session_file_read(
        &self,
        path: &std::path::Path,
        total_lines: u64,
        read_lines: u64,
        total_bytes: u64,
        read_bytes: u64,
        offset: u64,
        limit: Option<u64>,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "totalLines".to_string(),
            AnalyticsValue::Int(total_lines as i64),
        );
        md.insert(
            "readLines".to_string(),
            AnalyticsValue::Int(read_lines as i64),
        );
        md.insert(
            "totalBytes".to_string(),
            AnalyticsValue::Int(total_bytes as i64),
        );
        md.insert(
            "readBytes".to_string(),
            AnalyticsValue::Int(read_bytes as i64),
        );
        md.insert("offset".to_string(), AnalyticsValue::Int(offset as i64));
        if let Some(l) = limit {
            md.insert("limit".to_string(), AnalyticsValue::Int(l as i64));
        }
        if let Some(ext) = get_file_extension_for_analytics(path) {
            md.insert(
                "ext".to_string(),
                AnalyticsValue::String(Verified::assert_safe(ext).into_inner()),
            );
        }
        // messageID: TS spreads `...(messageId !== undefined && { messageID })`.
        // No assistant-message id is reachable from the Rust tool context, so the
        // key is faithfully omitted (the TS-undefined branch).
        let session_type = detect_session_file_type(path);
        md.insert(
            "is_session_memory".to_string(),
            AnalyticsValue::Bool(session_type == Some("session_memory")),
        );
        md.insert(
            "is_session_transcript".to_string(),
            AnalyticsValue::Bool(session_type == Some("session_transcript")),
        );
        self.ctx.bus.log_event(SESSION_FILE_READ, md).await;
    }

    /// `tengu_file_read_limits_override` (`FileReadTool.ts:512-515`) — fires only
    /// when `fileReadingLimits !== undefined`. The Rust `ToolUseContext` carries
    /// NO `fileReadingLimits` field, so this condition is never reachable in the
    /// port: the event name is registered, but the emit is a documented no-op
    /// (a faithful port of a never-true branch — no invented source). When TS
    /// fires it, the metadata is `hasMaxTokens`/`hasMaxSizeBytes` (bool); this
    /// helper accepts those for shape-parity even though no call site supplies a
    /// `Some(limits)`.
    async fn emit_file_read_limits_override(&self, limits: Option<(bool, bool)>) {
        // TS: `if (fileReadingLimits !== undefined) { logEvent(...) }`.
        let Some((has_max_tokens, has_max_size_bytes)) = limits else {
            // Unreachable in the port — `fileReadingLimits` is always `None`.
            return;
        };
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "hasMaxTokens".to_string(),
            AnalyticsValue::Bool(has_max_tokens),
        );
        md.insert(
            "hasMaxSizeBytes".to_string(),
            AnalyticsValue::Bool(has_max_size_bytes),
        );
        self.ctx.bus.log_event(FILE_READ_LIMITS_OVERRIDE, md).await;
    }

    /// Build + return the friendly file-not-found error — the ENOENT arm of
    /// `FileReadTool.ts:638-647`. TS builds the base message
    /// `File does not exist. ${FILE_NOT_FOUND_CWD_NOTE} ${getCwd()}.` and then
    /// appends a `" Did you mean ${x}?"` suffix, preferring the
    /// [`suggest_path_under_cwd`] "dropped repo folder" correction over the
    /// [`find_similar_file`] same-stem sibling:
    ///
    /// ```text
    /// let message = `File does not exist. ${FILE_NOT_FOUND_CWD_NOTE} ${getCwd()}.`
    /// if (cwdSuggestion)        message += ` Did you mean ${cwdSuggestion}?`
    /// else if (similarFilename) message += ` Did you mean ${similarFilename}?`
    /// ```
    ///
    /// The cwd is the LIVE cwd ([`Self::cwd_now`] — the injected shared cell if
    /// present, else `self.ctx.workspace`), canonicalized
    /// (`std::fs::canonicalize`, mirroring TS `getCwd()` returning a
    /// realpath-resolved cwd) with a fallback to the unresolved path so the
    /// message + the `suggest_path_under_cwd` prefix comparison both use the
    /// symlink-resolved form. The suffix is the byte-exact TS `" Did you mean
    /// {x}?"`.
    ///
    /// NotFound gate: this method is only ever invoked on the ENOENT branch of
    /// `call` (every call site is inside `if e.kind() == ErrorKind::NotFound`),
    /// so the cwd-note message applies to not-found errors only. A defensive
    /// guard keeps that explicit — a non-NotFound `err` (no current caller) is
    /// surfaced verbatim, mirroring TS's `throw error` for non-ENOENT.
    async fn file_not_found(
        &self,
        invocation_id: &str,
        canon: &std::path::Path,
        err: &std::io::Error,
    ) -> Result<ToolCallResult, ToolError> {
        // Faithful NotFound gate: only ENOENT gets the cwd-note message (TS
        // rethrows every other errno). Structurally every caller is already on
        // the NotFound branch; this makes the contract explicit + bulletproof.
        if err.kind() != std::io::ErrorKind::NotFound {
            self.emit_failed(invocation_id, "io_metadata").await;
            return Err(ToolError::Io(err.to_string()));
        }
        self.emit_failed(invocation_id, "io_metadata").await;
        // `getCwd()` analog: the LIVE cwd (`Ct()`), realpath-resolved (matching
        // TS's already-resolved cwd) with a fallback to the unresolved path.
        let live_cwd = self.cwd_now();
        Err(ToolError::Io(file_not_found_message(canon, &live_cwd)))
    }

    /// Process an image file and return it as multimodal content. The pixels
    /// reach the model INSIDE the tool_result content array (the dispatch loop
    /// derives `content_blocks` from the `{type:"image"}` result data — the port
    /// of claude-code's FileRead result-mapper `case "image"`); an optional
    /// `[Image: original …]` resize note rides as a following isMeta message
    /// (claude `newMessages: [Lr({content: y8t(dimensions), isMeta: true})]`).
    #[cfg(feature = "image-read")]
    async fn read_image_result(
        &self,
        invocation_id: &str,
        canon: &std::path::Path,
        bytes: Vec<u8>,
        original_size: u64,
        started: Instant,
    ) -> Result<ToolCallResult, ToolError> {
        // FT-09: `q_l`'s two pre-decode guards (cc-238.js @294617599), which run
        // BEFORE any decode/resize. Without them LingXi surfaced the `image`
        // crate's own text (`Image file is empty (0 bytes)` / `failed to decode
        // image: …`), which names neither the path nor what the bytes actually
        // are.
        if bytes.is_empty() {
            self.emit_failed(invocation_id, "image_empty").await;
            return Err(ToolError::Io(crate::image_read::format_image_empty(canon)));
        }
        if crate::image_read::sniff_image_media_type(&bytes).is_none() {
            self.emit_failed(invocation_id, "image_bad_magic").await;
            return Err(ToolError::Io(crate::image_read::format_image_bad_magic(
                canon, &bytes,
            )));
        }

        let processed = match crate::image_read::process_image(bytes) {
            Ok(p) => p,
            Err(e) => {
                self.emit_failed(invocation_id, "image_process").await;
                return Err(ToolError::Io(e));
            }
        };

        // claude-code FileReadTool image return (`{data: q, ...U && {newMessages:
        // [Lr({content: U, isMeta: true})]}}`): the IMAGE itself rides inside the
        // tool_result content array (the result mapper's `case "image"`, wired via
        // `content_blocks` in the dispatch loop), and ONLY the resize note `U =
        // y8t(q.file.dimensions)` is injected as a following isMeta user message —
        // and only when the image was resized (dimensions present).
        let new_messages = match processed.resized {
            Some((ow, oh, dw, dh)) => {
                let scale = f64::from(ow) / f64::from(dw.max(1));
                vec![lingxi_core::types::ConversationMessage::user_meta(
                    lingxi_core::types::MessageId::new(),
                    format!(
                        "[Image: original {ow}x{oh}, displayed at {dw}x{dh}. Multiply coordinates by {scale:.2} to map to original image.]"
                    ),
                )]
            }
            None => vec![],
        };

        // MIRROR the text-read success path's completion telemetry
        // (`emit_completed(invocation_id, bytes_read, duration_ms)` at the
        // `tengu_tool_read_completed` site). The image branch has no line/byte
        // slice counts (`emit_session_file_read` is text-only — TS fires
        // `tengu_session_file_read` only after a text read, not on images), so
        // we emit the simpler completion event with the original file size as the
        // `bytes_read` figure — telemetry is recorded, not silently skipped.
        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(invocation_id, original_size, duration_ms)
            .await;

        // binary `{type:"image", file:{base64, type:<mediaType>, originalSize
        // [, dimensions]}}` (no filePath) — `dimensions` present only when the
        // image was resized (claude `tbo`'s optional `dimensions`), and it is
        // what gates the resize-note `newMessages` above (`q.file.dimensions`).
        let mut file = serde_json::json!({
            "base64": processed.base64,
            "type": processed.media_type,
            "originalSize": original_size,
        });
        if let Some((ow, oh, dw, dh)) = processed.resized {
            file["dimensions"] = serde_json::json!({
                "originalWidth": ow,
                "originalHeight": oh,
                "displayWidth": dw,
                "displayHeight": dh,
            });
        }
        Ok(ToolCallResult {
            data: serde_json::json!({ "type": "image", "file": file }),
            // The image reaches the model INSIDE the tool_result content array
            // (the dispatch loop derives `content_blocks` from `data` — the port
            // of claude's result-mapper `case "image"`), so this text is
            // display-only (TUI/transcript); claude's wire form has no text.
            model_content: Some("[Image content provided in tool result.]".to_string()),
            new_messages,
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }

    /// Render `first..=last` of a PDF to page images and emit them on
    /// `new_messages` (claude-code FileRead `pages` path).
    #[cfg(feature = "pdf-render")]
    async fn read_pdf_pages_result(
        &self,
        invocation_id: &str,
        canon: &std::path::Path,
        original_size: u64,
        first: u32,
        last: u32,
        started: Instant,
    ) -> Result<ToolCallResult, ToolError> {
        let page_jpegs =
            match crate::pdf_render::render_pdf_pages(canon, original_size, first, last).await {
                Ok(p) => p,
                Err(e) => {
                    self.emit_failed(invocation_id, e.telemetry_code()).await;
                    return Err(ToolError::Io(e.message(&canon.display().to_string())));
                }
            };
        let (sources, data, model_content) =
            match build_pages_payload(canon, original_size, page_jpegs) {
                Ok(v) => v,
                Err(e) => {
                    self.emit_failed(invocation_id, "pdf_page_encode").await;
                    return Err(ToolError::Io(e));
                }
            };
        let msg = lingxi_core::types::ConversationMessage::user_with_images(
            lingxi_core::types::MessageId::new(),
            String::new(),
            sources,
        );
        self.emit_completed(
            invocation_id,
            original_size,
            started.elapsed().as_millis() as u64,
        )
        .await;
        Ok(ToolCallResult {
            data,
            model_content: Some(model_content),
            new_messages: vec![msg],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }

    /// Read a PDF as an inline document block (claude-code FileRead PDF path).
    /// Routing: `pages` ⇒ validate then defer to P4b extraction; else
    /// page-count > 10 ⇒ error; unsupported model ⇒ error; size > 20 MB ⇒
    /// too_large error; else inline document block on `new_messages`.
    ///
    /// The PDF bytes ride on `new_messages` as a `ContentBlock::Document`
    /// (the frozen tool-result content is text-only); the tool-result text is a
    /// short placeholder, mirroring [`Self::read_image_result`].
    #[cfg(feature = "pdf-read")]
    #[allow(clippy::too_many_arguments)]
    async fn read_pdf_result(
        &self,
        invocation_id: &str,
        canon: &std::path::Path,
        bytes: Vec<u8>,
        original_size: u64,
        pages: Option<String>,
        model: &str,
        started: Instant,
    ) -> Result<ToolCallResult, ToolError> {
        use crate::pdf_read::{
            is_pdf_supported, parse_pdf_page_range, pdf_page_count,
            PDF_AT_MENTION_INLINE_THRESHOLD, PDF_MAX_PAGES_PER_READ,
        };
        use base64::Engine;

        // Reject empty / non-PDF bytes BEFORE any inline document block can enter
        // history. claude-code (utils/pdf.ts readPDF) guards this deliberately: an
        // invalid PDF document block POISONS the conversation — every later API
        // call 400s with "The PDF specified was not valid" until /clear. Without
        // this, a non-PDF renamed `.pdf` makes `pdf_page_count` return None (skips
        // the page-count gate) and, if small + supported, inlines garbage.
        if bytes.is_empty() {
            self.emit_failed(invocation_id, "pdf_empty").await;
            return Err(ToolError::Io(format!(
                "PDF file is empty: {}",
                canon.display()
            )));
        }
        if !crate::pdf_read::looks_like_pdf(&bytes) {
            self.emit_failed(invocation_id, "pdf_invalid").await;
            return Err(ToolError::Io(format!(
                "File is not a valid PDF (missing %PDF- header): {}",
                canon.display()
            )));
        }

        // Model comes from the per-call `ToolUseContext` (`ctx.options.
        // main_loop_model`), the same source the text path uses for the
        // cyber-risk-mitigation gate. claude-code canonicalizes the model name
        // before `isPDFSupported`; LingXi compares the raw model id (the same
        // documented divergence as `MITIGATION_EXEMPT_MODELS`). LingXi's default
        // models are PDF-capable, so a stray non-canonical id only ever yields
        // `true` here unless it literally contains `claude-3-haiku`.
        let supported = is_pdf_supported(model);

        // `pages` parameter ⇒ ranged read. Validate the range and the per-read
        // page cap, then defer to P4b page-image extraction (not yet available).
        if let Some(ref p) = pages {
            let Some((first, last)) = parse_pdf_page_range(p) else {
                self.emit_failed(invocation_id, "pdf_pages_invalid").await;
                return Err(ToolError::Io(format!(
                    "Invalid pages parameter: \"{p}\". Use formats like \"1-5\", \"3\", or \"10-20\". Pages are 1-indexed."
                )));
            };
            // Open-ended `"N-"` ranges are treated as exceeding the cap (TS
            // resolves the open end against the doc's page count, which can be
            // any size; conservatively over-cap here so they take the error
            // path rather than silently uncapped extraction).
            let range_size = if last == u32::MAX {
                PDF_MAX_PAGES_PER_READ + 1
            } else {
                last - first + 1
            };
            if range_size > PDF_MAX_PAGES_PER_READ {
                self.emit_failed(invocation_id, "pdf_pages_too_many").await;
                return Err(ToolError::Io(format!(
                    "Page range \"{p}\" exceeds maximum of {PDF_MAX_PAGES_PER_READ} pages per request. Please use a smaller range."
                )));
            }
            #[cfg(feature = "pdf-render")]
            {
                return self
                    .read_pdf_pages_result(
                        invocation_id,
                        canon,
                        original_size,
                        first,
                        last,
                        started,
                    )
                    .await;
            }
            #[cfg(not(feature = "pdf-render"))]
            {
                self.emit_failed(invocation_id, "pdf_extraction_unavailable")
                    .await;
                // [RESIDUAL — message not in 2.1.206] port-only fallback for a
                // build WITHOUT the `pdf-render` feature; claude always ships
                // page extraction, so it has no analogous string. Desktop builds
                // enable `pdf-render` and never reach this.
                return Err(ToolError::Io(
                    "Reading specific PDF pages requires page extraction, which is not yet available. Read the whole PDF (omit pages) if it is small.".to_string(),
                ));
            }
        }

        // No `pages` ⇒ inline read. A parseable page count > 10 is too many to
        // read inline (TS: pdfinfo page-count gate; `None` skips the gate,
        // mirroring pdfinfo returning null).
        if let Some(count) = pdf_page_count(&bytes) {
            if count > PDF_AT_MENTION_INLINE_THRESHOLD {
                self.emit_failed(invocation_id, "pdf_too_many_pages").await;
                return Err(ToolError::Io(format!(
                    "This PDF has {count} pages, which is too many to read at once. Use the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). Maximum {PDF_MAX_PAGES_PER_READ} pages per request."
                )));
            }
        }

        // Unsupported model ⇒ refuse inline; tell the model to use a newer model
        // or the `pages` parameter (claude-code FileReadTool.ts:979-985).
        if !supported {
            self.emit_failed(invocation_id, "pdf_unsupported_model")
                .await;
            return Err(ToolError::Io(
                "Reading full PDFs is not supported with this model. Use a newer model (Sonnet 3.5 v2 or later), or use the pages parameter to read specific page ranges (e.g., pages: \"1-5\", maximum 20 pages per request). Page extraction requires poppler-utils: install with `brew install poppler` on macOS or `apt-get install poppler-utils` on Debian/Ubuntu.".to_string(),
            ));
        }
        // Supported model: inline up to PDF_TARGET_RAW_SIZE (20 MB), else
        // too_large (claude-code readPDF, utils/pdf.ts:60-68).
        if original_size > crate::pdf_read::PDF_TARGET_RAW_SIZE {
            self.emit_failed(invocation_id, "pdf_too_large").await;
            return Err(ToolError::Io(format!(
                "PDF file exceeds maximum allowed size of {}.",
                format_file_size(crate::pdf_read::PDF_TARGET_RAW_SIZE)
            )));
        }

        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let source = lingxi_core::types::DocumentSource::Base64 {
            media_type: "application/pdf".to_string(),
            // clone so the base64 also rides in the result `data.file.base64`.
            data: data.clone(),
        };
        let msg = lingxi_core::types::ConversationMessage::user_with_documents(
            lingxi_core::types::MessageId::new(),
            String::new(),
            vec![source],
        );
        // MIRROR the text/image success path's completion telemetry — the PDF
        // branch has no line/byte slice counts (`emit_session_file_read` is
        // text-only), so we emit the simpler completion event with the original
        // file size as the `bytes_read` figure.
        self.emit_completed(
            invocation_id,
            original_size,
            started.elapsed().as_millis() as u64,
        )
        .await;
        let model_content = format!(
            "PDF file read: {} ({})",
            canon.display(),
            format_file_size(original_size)
        );
        Ok(ToolCallResult {
            // binary `{type:"pdf", file:{filePath, base64, originalSize}}`; the
            // model receives the PDF as a document block via `new_messages`.
            data: serde_json::json!({
                "type": "pdf",
                "file": {
                    "filePath": canon.display().to_string(),
                    "base64": data,
                    "originalSize": original_size,
                },
            }),
            model_content: Some(model_content),
            new_messages: vec![msg],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["file_path"],
        "properties": {
            "file_path": { "type": "string", "description": "The absolute path to the file to read" },
            "offset": { "type": "integer", "minimum": 0, "description": "The line number to start reading from. Only provide if the file is too large to read at once" },
            "limit":  { "type": "integer", "minimum": 1, "description": "The number of lines to read. Only provide if the file is too large to read at once." },
            "pages":  { "type": "string", "description": "Page range for PDF files (e.g., \"1-5\", \"3\", \"10-20\"). Only applicable to PDF files. Maximum 20 pages per request." }
        }
    })
});

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<&str> {
        Some("read files, images, PDFs, notebooks")
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        // Byte-locked VERBATIM to claude-code `DESCRIPTION`
        // (`FileReadTool/prompt.ts:12`).
        "Read a file from the local filesystem.".to_string()
    }

    async fn prompt(&self, opts: &PromptOptions) -> String {
        // Model-gated, mirroring claude-code `Yhi(e,…){if(Dh(e))return SHORT;
        // return LONG}` (binary offset 195602689). `Dh(model)` selects the terse
        // variant for current-gen default models (opus-4-8 / fable-5 / mythos-5)
        // and whenever `LINGXI_SIMPLE_SYSTEM_PROMPT` is env-truthy; classic
        // models and `None` (the `Dh(undefined)` path) get the verbose one. The
        // predicate is shared with TodoWrite via `tool_api`.
        if tool_api::dh_simple_system_prompt(opts.model.as_deref()) {
            READ_PROMPT_SHORT.to_string()
        } else {
            READ_PROMPT_LONG.to_string()
        }
    }

    fn get_path(&self, input: &Value) -> Option<PathBuf> {
        input
            .get("file_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // `ctx` is consumed only by the PDF dispatch (model id for PDF support
        // gating), which is `pdf-read`-feature-gated. Without that feature the
        // text/image paths never read `ctx`; discard it so the default build does
        // not warn. (The former cyber-risk reminder used to read it here; that was
        // removed per parity verdict 12/14.)
        #[cfg(not(feature = "pdf-read"))]
        let _ = &ctx;
        let invocation_id = ulid_or_uuid();
        let file_path = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("file_path is required".into()))?;
        // Raw input values, preserved verbatim for the read-state registry
        // (TS `readFileState.set` stores the `offset`/`limit` as provided —
        // `undefined` when absent). `offset` below defaults to `1` only for
        // slicing; the registry records the un-defaulted `Option`.
        let input_offset = input.get("offset").and_then(Value::as_u64);
        let input_limit = input.get("limit").and_then(Value::as_u64);
        let offset = input_offset.unwrap_or(1);
        let limit = input_limit;

        let started = Instant::now();
        // Worktree parity plan (Task 3): a RELATIVE `file_path` resolves
        // against the CURRENT session cwd (`ctx.cwd()`, switchable by
        // `EnterWorktree`/`ExitWorktree`), not the frozen OS process cwd that
        // `std::fs::canonicalize` would otherwise consult below. An absolute
        // `file_path` (the documented/expected case) is unaffected.
        let task_output_id = if let Some(registry) = self.ctx.task_registry.as_ref() {
            registry.task_output_directory().await.and_then(|root| {
                lingxi_core::host::task_output::output_id(Path::new(&root), Path::new(file_path))
                    .map(str::to_owned)
            })
        } else {
            None
        };
        let path = resolve_against_cwd(PathBuf::from(file_path), &self.ctx.cwd());
        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, path, false) {
            Ok(path) => path,
            Err(message) => {
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::InvalidInput(message));
            }
        };
        self.emit_started(&invocation_id, &path).await;

        // #11: refuse blocking device/special files (claude-code `$3p` in
        // validateInput, BEFORE symlink resolution). Checked on the supplied
        // path string (device/proc paths are absolute, so no ~/cwd expansion is
        // needed). `${e}` is the original `file_path`.
        if is_device_file(file_path) {
            self.emit_failed(&invocation_id, "device_file").await;
            return Err(ToolError::InvalidInput(format!(
                "Cannot read '{file_path}': this device file would block or produce infinite output."
            )));
        }

        // `tengu_file_read_limits_override` (`FileReadTool.ts:511-516`): TS fires
        // this at the top of the read iff `fileReadingLimits !== undefined`. The
        // LingXi `ToolUseContext` has NO `fileReadingLimits` field — there is no
        // source for caller-overridden read limits — so the condition is never
        // true and the emit is a documented no-op (faithful port of a never-true
        // branch; the event NAME is still registered). `None` => never fires.
        self.emit_file_read_limits_override(None).await;

        // One snapshot of trusted_dirs for both containment checks below (the
        // primary path and the macOS screenshot alt-path retry), so they
        // can't straddle a swap between the two calls.
        let mut trusted_dirs = self.ctx.trusted_dirs();
        if let Some(root) =
            lingxi_core::host::teammate_plan::own_plan_file_root(ctx.agent_id.as_ref(), &path)
        {
            trusted_dirs.push(root);
        }

        let mut canon = match canonicalize_and_validate(&path, &trusted_dirs) {
            Ok(p) => p,
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::PathBlocked { path });
            }
        };

        // The permission check above approved `canon`; all bytes and metadata
        // now come from a no-follow handle opened relative to the approved
        // trusted root. This is deliberately not `tokio::fs::read(&canon)`:
        // reopening the canonical pathname would reintroduce a parent/leaf
        // symlink retarget window.
        //
        // TL-1: a ranged Read of a >256KB file must not `read_to_end` the
        // whole body. Hint from a metadata stat; on success we keep the
        // no-follow handle and stream lines. Anything else falls through to
        // the existing snapshot path.
        let skip_full_load = input_limit.is_some()
            && !is_image_path(&canon)
            && !is_pdf_path(&canon)
            && !std::path::Path::new(file_path)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"))
            && std::fs::metadata(&canon)
                .map(|metadata| metadata.len() > MAX_FILE_READ_SIZE)
                .unwrap_or(false);
        let mut stream_source: Option<std::fs::File> = None;
        let snapshot = 'load: {
            if skip_full_load {
                if let Ok(file) = open_rooted_file(&path, &canon, &trusted_dirs) {
                    if let Ok(metadata) = file.metadata() {
                        if metadata.len() > MAX_FILE_READ_SIZE {
                            let size = metadata.len();
                            let modified = metadata.modified().ok();
                            stream_source = Some(file);
                            break 'load lingxi_core::host::rooted_fs::RootedFileSnapshot {
                                bytes: Vec::new(),
                                size,
                                modified,
                                mode: None,
                            };
                        }
                    }
                }
            }
            match read_rooted_snapshot(&path, &canon, &trusted_dirs) {
                Ok(snapshot) => snapshot,
                Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
                    lingxi_core::host::FsError::NotFound(_),
                )) => {
                    // Missing-file UX (`FileReadTool.ts:608-649`). On ENOENT TS first
                    // tries the macOS-screenshot AM/PM space variant (regular space ⇄
                    // thin space, U+202F), then reports the friendly message if that
                    // variant is also absent.
                    if let Some(alt) = get_alternate_screenshot_path(&canon) {
                        if let Ok(alt_canon) = canonicalize_and_validate(&alt, &trusted_dirs) {
                            match read_rooted_snapshot(&alt, &alt_canon, &trusted_dirs) {
                                Ok(snapshot) => {
                                    canon = alt_canon;
                                    snapshot
                                }
                                Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
                                    lingxi_core::host::FsError::NotFound(_),
                                )) => {
                                    let not_found =
                                        std::io::Error::from(std::io::ErrorKind::NotFound);
                                    return self
                                        .file_not_found(&invocation_id, &canon, &not_found)
                                        .await;
                                }
                                Err(error) => {
                                    self.emit_failed(&invocation_id, "io_metadata").await;
                                    return Err(ToolError::Io(error.to_string()));
                                }
                            }
                        } else {
                            let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
                            return self
                                .file_not_found(&invocation_id, &canon, &not_found)
                                .await;
                        }
                    } else {
                        let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
                        return self
                            .file_not_found(&invocation_id, &canon, &not_found)
                            .await;
                    }
                }
                Err(lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged)
                | Err(
                    lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged,
                ) => {
                    self.emit_failed(&invocation_id, "symlink_resolution_changed")
                        .await;
                    return Err(ToolError::InvalidInput(symlink_resolution_changed_message(
                        file_path,
                    )));
                }
                Err(lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile)
                    if std::path::Path::new(file_path)
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb")) =>
                {
                    self.emit_failed(&invocation_id, "notebook_not_regular_file")
                        .await;
                    return Err(ToolError::Io(
                        crate::notebook_read::NOTEBOOK_NOT_REGULAR_FILE.to_string(),
                    ));
                }
                Err(error) => {
                    self.emit_failed(&invocation_id, "io_metadata").await;
                    return Err(ToolError::Io(error.to_string()));
                }
            }
        };
        let size = snapshot.size;
        // Floor-truncated mtime in ms, matching TS `Math.floor(mtimeMs)` for
        // the read-state registry (`readFileState.set`). A missing mtime
        // (rare; e.g. platforms without mtime) falls back to the epoch (`0`).
        let mtime_ms = snapshot
            .modified
            .map(tool_api::read_file_state::mtime_ms_floor)
            .unwrap_or(0);

        // Read dedup (`FileReadTool.ts:536-573`): if this EXACT range was
        // already read (a prior `Read`, full view, same offset+limit) and the
        // file's mtime is unchanged on disk, return the byte-locked
        // `file_unchanged` stub instead of re-sending the content. The earlier
        // Read tool_result is still in context; two full copies waste
        // cache_creation tokens on every later turn. Gated on:
        //   * a recorded entry that came from a `Read` (`from_read`; TS
        //     `existingState.offset !== undefined`) — never an Edit/Write
        //     post-write entry, whose content/mtime reflect the post-edit
        //     state and would misdirect the model;
        //   * a FULL view (`offset`/`limit` both `None`; TS
        //     `!existingState.isPartialView`, approximated as in the staleness
        //     guard);
        //   * the SAME range (`entry.offset == input_offset &&
        //     entry.limit == input_limit`; TS `rangeMatch`);
        //   * unchanged mtime (`mtime_ms == entry.mtime_ms`; TS
        //     `mtimeMs === existingState.timestamp`).
        // The GB killswitch (`tengu_read_dedup_killswitch`) is unported — LingXi
        // has no GrowthBook; dedup is always enabled (3P default = killswitch
        // off).
        //
        // The SEEDED branch (@235741459) runs FIRST and is a different shape:
        //   if(_&&_.seededFromContext&&!_.isPartialView&&t===1&&r===void 0)
        //     if(await b0e(f)===_.timestamp) → {type:"file_unchanged", …,
        //                                      source:"seeded"}
        // It covers memory files (LINGXI.md / rules) seeded into the registry at
        // session start by `ConversationOrchestrator::seed_memory_read_state` —
        // content the model already received inside the system prompt's memory
        // block, so re-sending it is pure waste. Two deltas from the branch
        // below, both deliberate:
        //   * it does NOT consult `from_read` — a seeded entry has
        //     `from_read: false` by construction, and the oracle's gate never
        //     mentions `offset`/`rangeMatch` here;
        //   * it gates on the DEFAULTED `offset` (`t===1`), not the raw
        //     `input_offset`, so an explicit `offset: 1` still dedups — whereas
        //     the non-seeded `range_match` below would refuse it.
        if let Some(entry) = tool_api::read_file_state::get(&self.ctx.read_file_state, &canon) {
            // #13: `tengu_file_read_reread` fires whenever the file ALREADY has a
            // read-state entry (the `if(m)` in claude-code), BEFORE the dedup
            // short-circuit. `priorOp` is a THREE-way (@235740900:
            // `m.seededFromContext?"seeded":m.offset===void 0?"edit_write":"read"`)
            // with `seeded` tested FIRST; LingXi's `from_read` flag is the
            // faithful Read-vs-Edit/Write discriminator for the other two arms
            // (a full Read also has `offset==None`, so `offset` alone is wrong).
            self.emit_file_read_reread(if entry.seeded_from_context {
                "seeded"
            } else if entry.from_read {
                "read"
            } else {
                "edit_write"
            })
            .await;
            if entry.seeded_from_context
                && !entry.is_partial_view
                && offset == 1
                && input_limit.is_none()
                && mtime_ms == entry.mtime_ms
            {
                self.emit_file_read_dedup(&canon, Some("seeded")).await;
                self.emit_completed(&invocation_id, 0, started.elapsed().as_millis() as u64)
                    .await;
                return Ok(ToolCallResult {
                    // binary `{type:"file_unchanged", file:{filePath:f},
                    // source:"seeded"}` (@235741583) — `f` is `fullFilePath`,
                    // the canonicalized path on the Rust side.
                    data: json!({
                        "type": "file_unchanged",
                        "file": { "filePath": canon.display().to_string() },
                        "source": "seeded",
                    }),
                    // Renderer @235742950:
                    // `content = e.source==="seeded" ? vou(e.file.filePath) : Eou()`.
                    // The stub names the LEXICAL path, not `canon`. The oracle's
                    // `f` here is `Li(e)` (@226620214), which is pure
                    // `path.normalize`/`path.resolve` — it does NOT resolve
                    // symlinks. LingXi's `canonicalize_and_validate` does, for
                    // sandbox containment, so under a symlinked cwd `canon`
                    // would read `/private/var/...` while the memory block that
                    // this stub points at printed `/var/...`. Since the whole
                    // point of the stub is "see the block above", it has to name
                    // the path the block used.
                    model_content: Some(format_file_unchanged_seeded(&path)),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                });
            }
            let is_full_view = entry.offset.is_none() && entry.limit.is_none();
            let range_match = entry.offset == input_offset && entry.limit == input_limit;
            if entry.from_read && is_full_view && range_match && mtime_ms == entry.mtime_ms {
                // `tengu_file_read_dedup` (`FileReadTool.ts:559-561`) — fired in
                // the dedup short-circuit, before returning the `file_unchanged`
                // stub. Metadata: `ext` only when present (the analytics ext of
                // the resolved path). TS uses `fullFilePath` (`expandPath`); the
                // Rust analog is the canonicalized path.
                self.emit_file_read_dedup(&canon, None).await;
                // Behaves like the TS early return: the model sees the stub via
                // `model_content`; the TUI payload `content` mirrors it (there
                // is no fresh file body to render). `total_lines`/`line_range`
                // are omitted — this is the `file_unchanged` result variant.
                self.emit_completed(&invocation_id, 0, started.elapsed().as_millis() as u64)
                    .await;
                return Ok(ToolCallResult {
                    // binary `{type:"file_unchanged", file:{filePath}}` (@235741168);
                    // the stub rides on `model_content`, not inside `data`. The
                    // oracle's third field `source:"seeded"` is set ONLY by the
                    // seeded branch above, so it is omitted here — matching the
                    // oracle's non-seeded return exactly.
                    data: json!({
                        "type": "file_unchanged",
                        "file": { "filePath": canon.display().to_string() },
                    }),
                    // Renderer @235742950 picks `Eou()` = `Sou` for a non-seeded
                    // `file_unchanged`. The long form `FOg` has no producer in
                    // 2.1.220 — see [`FILE_UNCHANGED_STUB`].
                    model_content: Some(FILE_UNCHANGED_SHORT.to_string()),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                });
            }
        }

        // Image files route to the multimodal image path (claude-code routes by
        // extension to readImageWithTokenBudget, bypassing the text size cap).
        let is_image = cfg!(feature = "image-read") && is_image_path(&canon);

        // PDF files route to the document path (claude-code routes by extension
        // to the PDF reader, which applies its own PDF size gates — 20MB inline /
        // 100MB extraction — not the 256KB text cap). Gated on the feature so the
        // cap still applies — and PDFs still hit the binary guard — when
        // `pdf-read` is off.
        let is_pdf = cfg!(feature = "pdf-read") && is_pdf_path(&canon);

        // Binary-extension gate (`FileReadTool.ts:469-482`). A file whose
        // extension is in `BINARY_EXTENSIONS` is rejected as binary *before* any
        // read — EXCEPT the extensions the tool renders natively (the 5 image
        // exts + `.pdf`), which TS excludes via `!isPDFExtension && !IMAGE_EXTENSIONS
        // .has(...)`. The exclusion uses the raw, feature-INDEPENDENT extension
        // predicates (`is_image_path`/`is_pdf_path`) so a `.png`/`.pdf` is excluded
        // even when its render feature is off — exactly like TS, which has no
        // feature flags (an excluded image/PDF with the feature off then falls
        // through to the NUL-scan, which still catches its binary bytes). This is
        // the PRIMARY binary detection (by extension); the NUL-scan below is the
        // fallback for extensionless / non-listed binaries (`FileReadTool.ts` reads
        // the bytes then scans — here `looks_binary`).
        if has_binary_extension(&canon) && !is_image_path(&canon) && !is_pdf_path(&canon) {
            self.emit_failed(&invocation_id, "binary_file").await;
            return Err(ToolError::Io(format_binary(&canon)));
        }

        // FT-04: notebook stat guards. The oracle's `Sgm` dispatches the
        // `.ipynb` branch FIRST (`if(BXt(n)){let F=await K0f(n) …}`,
        // cc-238.js @294612195), and `K0f` (@289994340) stats the path before
        // touching its bytes:
        //   if(!n.isFile()&&!n.isDirectory())throw new Wur(<NOT_REGULAR_FILE>);
        //   if(n.size>aV)throw new Wur(l8n());
        //   let o=await …readFileBytes(r,aV+1); if(o.length>aV)throw new Wur(l8n());
        // Both literals are NEW in 2.1.238 (0 hits in 2.1.220). They run before
        // the generic 256 KB cap below so a >100 MB notebook reports the
        // notebook message, not the generic one.
        //
        // Divergence recorded, NOT fixed here: upstream the notebook branch also
        // preempts the generic `maxSizeBytes` cap entirely (it compares the
        // SERIALIZED cells JSON, not the file size — see the `cells_json_bytes`
        // check further down). LingXi still applies the generic cap to `.ipynb`
        // at `size > MAX_FILE_READ_SIZE` below; realigning that ordering is a
        // separate finding.
        if std::path::Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
            && size > crate::notebook_read::MAX_NOTEBOOK_READ_SIZE
        {
            self.emit_failed(&invocation_id, "notebook_too_large").await;
            return Err(ToolError::Io(
                crate::notebook_read::notebook_too_large_message(),
            ));
        }

        // TS applies the byte cap ONLY when no `limit` is supplied
        // (`readFileInRange(..., limit === undefined ? maxSizeBytes : undefined)`
        // — FileReadTool.ts:1026). A ranged read (offset+limit) of a >256KB file
        // must succeed and return just the requested lines, so the cap is gated
        // on `input_limit.is_none()`. A no-limit oversize read still errors with
        // the byte-locked template (fixture-pinned `error_template`).
        if !is_image && !is_pdf && input_limit.is_none() && size > MAX_FILE_READ_SIZE {
            self.emit_failed(&invocation_id, "file_too_large").await;
            return Err(ToolError::Io(format_too_large(&canon, size)));
        }

        let ext = std::path::Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let max_output_tokens = effective_max_output_tokens();

        if !is_image
            && !is_pdf
            && ext.as_deref() != Some("ipynb")
            && input_limit.is_some()
            && size > MAX_FILE_READ_SIZE
        {
            let streamed = match if let Some(file) = stream_source.take() {
                read_text_range_streaming(
                    file,
                    &canon,
                    offset,
                    input_limit.expect("checked is_some"),
                    ext.as_deref(),
                    max_output_tokens,
                )
                .await
            } else {
                read_text_range_streaming(
                    snapshot.bytes.as_slice(),
                    &canon,
                    offset,
                    input_limit.expect("checked is_some"),
                    ext.as_deref(),
                    max_output_tokens,
                )
                .await
            } {
                Ok(result) => result,
                Err(e) => {
                    self.emit_failed(&invocation_id, "streaming_range_read")
                        .await;
                    return Err(e);
                }
            };

            let mut slice = streamed.content;
            let total_lines = streamed.total_lines;
            let line_range_start = offset;
            let read_lines = streamed.read_lines;
            let token_estimate = rough_token_count_estimation_for_file_type(&slice, ext.as_deref());
            if token_estimate != 0
                && token_estimate > max_output_tokens / 4
                && token_estimate > max_output_tokens
            {
                self.emit_failed(&invocation_id, "max_tokens_exceeded")
                    .await;
                return Err(ToolError::Io(format_max_tokens_exceeded(
                    token_estimate,
                    max_output_tokens,
                )));
            }

            let duration_ms = started.elapsed().as_millis() as u64;
            self.emit_completed(&invocation_id, size, duration_ms).await;
            tool_api::read_file_state::set(
                &self.ctx.read_file_state,
                canon.clone(),
                tool_api::read_file_state::ReadFileEntry {
                    content: slice.clone(),
                    mtime_ms,
                    offset: input_offset,
                    limit: input_limit,
                    from_read: true,
                    seeded_from_context: false,
                    is_partial_view: false,
                },
            );
            self.emit_session_file_read(
                &canon,
                total_lines,
                read_lines,
                size,
                slice.len() as u64,
                offset,
                input_limit,
            )
            .await;

            let model_content = if slice.is_empty() {
                if total_lines == 0 {
                    EMPTY_FILE_WARNING.to_string()
                } else {
                    format_offset_beyond_eof(offset, total_lines)
                }
            } else {
                add_line_numbers(&slice, offset)
            };
            let mut file = json!({
                "filePath": canon.display().to_string(),
                "content": std::mem::take(&mut slice),
                "numLines": read_lines,
                "startLine": line_range_start,
                "totalLines": total_lines,
            });
            if let Some(id) = &task_output_id {
                file["taskId"] = json!(id);
            }
            return Ok(ToolCallResult {
                data: json!({ "type": "text", "file": file }),
                model_content: Some(model_content),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            });
        }

        let bytes = snapshot.bytes;

        #[cfg(feature = "image-read")]
        if is_image {
            return self
                .read_image_result(&invocation_id, &canon, bytes, size, started)
                .await;
        }

        #[cfg(feature = "pdf-read")]
        if is_pdf {
            let pages = input
                .get("pages")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            return self
                .read_pdf_result(
                    &invocation_id,
                    &canon,
                    bytes,
                    size,
                    pages,
                    &ctx.options.main_loop_model,
                    started,
                )
                .await;
        }

        let head = &bytes[..bytes.len().min(NUL_SCAN_WINDOW)];
        if looks_binary(head) {
            self.emit_failed(&invocation_id, "binary_file").await;
            return Err(ToolError::Io(format_binary(&canon)));
        }

        let content = match decode_utf8_strict(&bytes) {
            Ok(s) => s,
            Err(_) => {
                self.emit_failed(&invocation_id, "non_utf8").await;
                return Err(ToolError::Io(format!(
                    "File {} is not valid UTF-8",
                    canon.display()
                )));
            }
        };

        // Notebook (`.ipynb`) structured-cell read (`FileReadTool.ts:822-863`).
        // A notebook is parsed into an ARRAY of structured cells — NOT the
        // line-numbered plain text below — so the model sees per-cell
        // `{ cellType, source, execution_count?, cell_id, language?, outputs? }`
        // exactly as TS's `readNotebook`. The TUI `data` carries the structured
        // `cells`; the model-facing string (`model_content`) is the text-block
        // projection of TS's `mapNotebookCellsToToolResult`. The `ext` is taken
        // from the ORIGINAL input path (TS `path.extname(file_path)`), matching
        // the same case-insensitive `.ipynb` test FileEditTool uses to route to
        // NotebookEdit. A parse failure surfaces the JSON error (TS's
        // `jsonParse` throws → propagates out of `call`).
        if ext.as_deref() == Some("ipynb") {
            let cells = match crate::notebook_read::read_notebook(&content) {
                Ok(c) => c,
                Err(e) => {
                    self.emit_failed(&invocation_id, "notebook_parse").await;
                    return Err(ToolError::Io(e));
                }
            };
            let model_content = crate::notebook_read::render_cells_model_text(&cells);

            // Serialized cells JSON — TS's `cellsJson = jsonStringify(cells)`
            // (`FileReadTool.ts:824`). One serialization, reused for the byte cap,
            // the token gate, and the registry entry (matches TS's single
            // `cellsJson`).
            let cells_json = serde_json::to_string(&cells).unwrap_or_default();

            // Notebook byte-size cap (`FileReadTool.ts:826-836`): if the serialized
            // cells JSON exceeds `maxSizeBytes` (the 256 KB default — LingXi has no
            // `fileReadingLimits` override), error with the jq-suggestion message
            // VERBATIM from the binary (`$Ka()` @2.1.238), before the token gate /
            // state record. 2.1.238 de-parameterised the PATH: 2.1.220 interpolated
            // the real path into `cat "<path>"` (quoted), 2.1.238 emits the literal
            // placeholder `cat <notebook_path>` (unquoted) — verified at the oracle
            // (238 @113219062 vs 220 @109111686). The SHELL TOOL NAME is still an
            // interpolation slot in BOTH builds, not the literal `Bash`; LingXi
            // renders that slot as "a registered shell tool" repo-wide because it
            // registers more than one shell tool (Bash + PowerShell). That is an
            // intentional divergence, asserted by `grep.rs`'s
            // `description_is_byte_faithful` and `agent/src/builtins.rs:722` —
            // do NOT "align" it to `Bash`. Both sizes use `format_file_size`.
            let cells_json_bytes = cells_json.len() as u64;
            if cells_json_bytes > MAX_FILE_READ_SIZE {
                self.emit_failed(&invocation_id, "notebook_too_large").await;
                return Err(ToolError::Io(format!(
                    "Notebook content ({}) exceeds maximum allowed size ({}). {}",
                    format_file_size(cells_json_bytes),
                    format_file_size(MAX_FILE_READ_SIZE),
                    crate::notebook_read::NOTEBOOK_JQ_GUIDANCE,
                )));
            }

            // Token-budget gate on the cells JSON — TS runs
            // `validateContentTokens(cellsJson, ext, maxTokens)`
            // (`FileReadTool.ts:838`) on the notebook path too, after the
            // byte-size check and before recording state. `ext` is `"ipynb"`
            // (bytesPerToken 4).
            if let Err(msg) = validate_content_tokens(&cells_json, Some("ipynb"), max_output_tokens)
            {
                self.emit_failed(&invocation_id, "max_tokens_exceeded")
                    .await;
                return Err(ToolError::Io(msg));
            }

            let duration_ms = started.elapsed().as_millis() as u64;
            self.emit_completed(&invocation_id, bytes.len() as u64, duration_ms)
                .await;

            // Record the read — TS stores the serialized cells JSON as the
            // entry `content` (`FileReadTool.ts:842-847`) with the DEFAULTED
            // offset/limit. To keep the registry's full-view discriminator
            // (used by the staleness guard + dedup) consistent with the rest of
            // the Rust port, a no-range notebook read records `offset`/`limit`
            // as the verbatim (un-defaulted) input — `None` for a full read —
            // matching `read.rs`'s text path. The stored `content` is the
            // cells JSON (so a same-range re-read dedups byte-for-byte).
            tool_api::read_file_state::set(
                &self.ctx.read_file_state,
                canon.clone(),
                tool_api::read_file_state::ReadFileEntry {
                    content: cells_json,
                    mtime_ms,
                    offset: input_offset,
                    limit: input_limit,
                    from_read: true,
                    seeded_from_context: false,
                    is_partial_view: false,
                },
            );

            return Ok(ToolCallResult {
                // binary `{type:"notebook", file:{filePath, cells}}`; the rendered
                // cells text rides on `model_content`.
                data: json!({
                    "type": "notebook",
                    "file": { "filePath": canon.display().to_string(), "cells": cells },
                }),
                model_content: Some(model_content),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            });
        }

        let all_lines: Vec<&str> = content.split_inclusive('\n').collect();
        // FILE.4: `total_lines` must match TS `readFileInRange.totalLines`
        // (`utils/readFileInRange.ts:188`) = (number of '\n') + 1. TS does an
        // UNCONDITIONAL post-loop `lineIndex++` that counts the (possibly empty)
        // final fragment after the last newline, so a TRAILING newline adds a
        // phantom final line (e.g. "a\nb\nc\n" => 4, not the natural 3). The prior
        // `split_inclusive().len()` undercounted trailing-newline files by one in
        // the model-facing `total_lines` field + the offset-beyond-EOF warning.
        // An empty file stays 0 so the byte-locked EMPTY_FILE_WARNING branch still
        // fires (TS's empty path is upstream of readFileInRange and not reproduced
        // here). NOTE: `all_lines` (split_inclusive) is what the range SLICING below
        // uses — only this model-facing count is TS-aligned.
        let total_lines = if content.is_empty() {
            0
        } else {
            content.bytes().filter(|&b| b == b'\n').count() as u64 + 1
        };
        let start_idx = (offset.saturating_sub(1) as usize).min(all_lines.len());
        // NO line cap. claude-code's runtime passes `maxLines = limit` VERBATIM
        // (no `?? MAX_LINES_TO_READ` default), so an UNDEFINED `limit` reads to
        // EOF; only an explicit `limit` bounds the end index. The "up to 2000
        // lines" wording in the prompt is advisory text, NOT a runtime cap.
        let end_idx = match limit {
            Some(l) => (start_idx + l as usize).min(all_lines.len()),
            None => all_lines.len(),
        };
        let mut slice: String = all_lines[start_idx..end_idx].concat();
        let line_range_start = offset;
        // `read_lines` is the model-facing line count of the returned slice; it
        // starts as the raw slice line count and is overwritten by a graceful
        // truncation (`S` in claude-code).
        let mut read_lines = (end_idx - start_idx) as u64;

        // Token-budget gate — `validateContentTokens(content, ext, maxTokens)`
        // (`FileReadTool.ts`). LingXi has no `fileReadingLimits`, so the budget
        // is the default. Over-budget handling diverges by whether this is a
        // FULL read (`k = offset <= 1 && limit === undefined && pages ===
        // undefined`): a full read is GRACEFULLY token-truncated (the catch
        // block ports claude-code's `if(L instanceof Fae && k)` branch); an
        // explicit-range read RE-THROWS as a hard error (`else throw L`).
        //
        // `pages` is never set on the text path (it's a PDF-only input), so the
        // `k` flag here is `offset <= 1 && limit is None`.
        let token_estimate = rough_token_count_estimation_for_file_type(&slice, ext.as_deref());
        let mut partial_note: Option<String> = None;
        // Mirror `validateContentTokens`'s early-pass band: skip when 0 or
        // `<= maxTokens/4`; otherwise the offline fallback is `effectiveCount ==
        // estimate`, over budget iff `estimate > maxTokens`.
        if token_estimate != 0
            && token_estimate > max_output_tokens / 4
            && token_estimate > max_output_tokens
        {
            let is_full_read = offset <= 1 && limit.is_none();
            if is_full_read {
                // Graceful truncation (`T=U, S=lineCount, v=S, R=note`).
                let trunc = truncate_to_token_budget(
                    &slice,
                    token_estimate,
                    max_output_tokens,
                    total_lines,
                    &canon.display().to_string(),
                );
                slice = trunc.content;
                read_lines = trunc.line_count;
                partial_note = Some(trunc.note);
            } else {
                self.emit_failed(&invocation_id, "max_tokens_exceeded")
                    .await;
                return Err(ToolError::Io(format_max_tokens_exceeded(
                    token_estimate,
                    max_output_tokens,
                )));
            }
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, bytes.len() as u64, duration_ms)
            .await;

        // Record the read into the shared read-state registry — 1:1 with TS
        // `readFileState.set(fullFilePath, {content, timestamp, offset,
        // limit})` (`FileReadTool.ts:1032`). `content` is the range-limited
        // slice TS stores (from `readFileInRange`), `mtime_ms` is floored, and
        // `offset`/`limit` are the verbatim (un-defaulted) input values. The
        // key is the canonicalized absolute path. Behavior-neutral side-effect:
        // nothing reads this map yet (staleness guards + Read dedup are later
        // batches), so the tool's result shape is unchanged.
        tool_api::read_file_state::set(
            &self.ctx.read_file_state,
            canon.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: slice.clone(),
                mtime_ms,
                offset: input_offset,
                limit: input_limit,
                // This entry is the product of a `Read` — the dedup gate
                // (`FileReadTool.ts:550` `offset !== undefined`) only short-
                // circuits against Read-sourced entries.
                from_read: true,
                // A Read never seeds; only the orchestrator's
                // `seed_memory_read_state` does.
                seeded_from_context: false,
                // `...x!==void 0&&{isPartialView:!0}` (@235732534). `x` is the
                // truncation NOTE, assigned only inside
                // `catch(U){if(U instanceof ZVe && O){…}}` — the token-cap
                // overflow on a full-file read — so `isPartialView` means
                // exactly "this read was token-truncated", which is
                // `partial_note.is_some()` here.
                //
                // Load-bearing: `Aze` (@232453039) returns false for a partial
                // view, and the Edit/Write guards (`if(!p||p.isPartialView)`,
                // @232481342 / @232489643) then refuse to treat the file as
                // read. Hard-coding `false` let a truncated read masquerade as a
                // full one.
                is_partial_view: partial_note.is_some(),
            },
        );

        // `tengu_session_file_read` (`FileReadTool.ts:1069-1083`) — fired after a
        // successful TEXT read, mirroring TS's site (after `readFileState.set`,
        // before returning the data). NOT fired on the notebook path (TS returns
        // before this site) nor on images/PDFs. The byte/line counts mirror
        // `readFileInRange`'s return: `totalBytes = Buffer.byteLength(text)` →
        // full content byte length; `readBytes = R!==void 0 ? byteLen(T) : _` →
        // the (possibly truncated) slice's byte length; `readLines = S` → the
        // model-facing line count (truncation overwrites it). `offset` is the
        // defaulted offset; `limit` only when supplied. See
        // [`emit_session_file_read`] for the `ext` / `messageID` (omitted) /
        // session-flag handling.
        self.emit_session_file_read(
            &canon,
            total_lines,
            read_lines,
            content.len() as u64,
            slice.len() as u64,
            offset,
            input_limit,
        )
        .await;

        // Model-facing serialization (FILE.A). `content` above stays the RAW
        // slice — that is what the TUI renders (`emit_tool_result` payload). The
        // model instead sees `model_content`: cat -n line numbers for non-empty
        // reads, or the byte-locked empty / offset-beyond-EOF
        // `<system-reminder>` warning otherwise. This mirrors
        // claude-code's `FileReadTool` mapper (`FileReadTool.ts:692-714`), where
        // the model string diverges from the UI chrome.
        let model_content = if slice.is_empty() {
            if total_lines == 0 {
                EMPTY_FILE_WARNING.to_string()
            } else {
                format_offset_beyond_eof(offset, total_lines)
            }
        } else {
            // FT-08 (delivery half): the `[Truncated: PARTIAL view — …]` banner
            // is NOT part of the tool_result. The oracle's `Sgm` ends with
            // `if(R!==void 0)daf(L,R)` (@294616079) — it stashes the banner in a
            // WeakMap keyed on the RESULT DATA object and returns `{data:L}`
            // only; the tool_result text is the cat -n render alone. The
            // dispatcher then re-reads it (@294300871:
            // `if(e.name===Ns&&re.data&&typeof re.data==="object"){let pt=paf(re.data);
            //  if(pt!==void 0)S.push({message:gc({type:"read_truncation_notice",banner:pt,toolUseID:t})})}`)
            // and the attachment renderer turns it into its OWN meta user
            // message (@296736309:
            // `read_truncation_notice:(e)=>Zy([kn({content:pze(e.banner),isMeta:!0})])`).
            // LingXi used to concatenate the note onto `model_content` after a
            // blank line, which put the banner INSIDE the tool_result block —
            // different bytes, and it made the file's own content and the
            // harness's warning indistinguishable to the model. The banner now
            // rides `new_messages` (see below), the same seam the image-resize
            // note already uses.
            //
            // NOTE (parity verdict 12/14): no per-read cyber-risk/malware reminder
            // is appended — claude-code v2.1.183 does not have one; the
            // defensive-security guidance lives in the system-prompt body instead.
            add_line_numbers(&slice, offset)
        };

        // claude-code FileRead result `data` (2.1.191): `{type, file:{…}}` — pure
        // metadata; the cat -n model render rides on `model_content`. The text
        // `file` is `{filePath, content, numLines, startLine, totalLines,
        // truncatedByTokenCap?}` — `truncatedByTokenCap` is added only on a
        // token-cap-shrunk read (binary spreads `{truncatedByTokenCap:!0}` last).
        let mut file = json!({
            "filePath": canon.display().to_string(),
            "content": slice,
            "numLines": read_lines,
            "startLine": line_range_start,
            "totalLines": total_lines,
        });
        if let Some(id) = &task_output_id {
            file["taskId"] = json!(id);
        }
        if partial_note.is_some() {
            file["truncatedByTokenCap"] = json!(true);
        }
        // FT-08: the truncation banner as its OWN `isMeta` user message,
        // appended after this tool_result — the port of the oracle's
        // `read_truncation_notice` attachment (producer @294300871, renderer
        // @296736309). `turn_loop`'s `injected_messages` seam (the site that
        // already delivers the image-resize note) appends it right after the
        // batch's tool_result user message, which is where `Zy([kn({…})])`
        // lands it upstream. Empty for every non-truncated read, so the locked
        // turn-loop fixtures stay byte-identical.
        let new_messages = match &partial_note {
            Some(note) => vec![lingxi_core::types::ConversationMessage::user_meta(
                lingxi_core::types::MessageId::new(),
                escape_attachment_banner(note),
            )],
            None => vec![],
        };

        Ok(ToolCallResult {
            data: json!({ "type": "text", "file": file }),
            model_content: Some(model_content),
            new_messages,
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Oracle `pze` (`@285128585`) — the escaper the `read_truncation_notice`
/// renderer runs the banner through before it becomes message text:
///
/// ```text
/// function pze(e){return ktp(e.replaceAll("&","&amp;").replaceAll("<","&lt;").replaceAll(">","&gt;"))}
/// function ktp(e){return e.replace(cYb,(t)=>`&#${t.charCodeAt(0)};`)}
/// cYb=/[\x00-\x1f\x7f-\x9f\u2028\u2029]/g
/// ```
///
/// It is NOT applied to the sibling attachments (`nested_memory` interpolates
/// its content raw), so this is specific to this banner. For an ordinary path
/// it is the identity; it only bites when the file path carries `&`, `<`, `>`,
/// a control character, or U+2028/U+2029 — exactly the cases where an unescaped
/// path could forge markup inside the injected message.
fn escape_attachment_banner(banner: &str) -> String {
    let mut out = String::with_capacity(banner.len());
    for ch in banner.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // `ktp`: C0, DEL, C1, and the two Unicode line separators become
            // decimal numeric character references.
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{2028}' | '\u{2029}' => {
                use std::fmt::Write as _;
                let _ = write!(out, "&#{};", ch as u32);
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tempfile::TempDir;
    use tool_api::test_support::{fresh_ctx, fresh_tx, make_dummy_fs};

    static READ_LIMIT_ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn is_device_file_matches_claude_set() {
        // `/dev` set members ($3p / U3p).
        for p in [
            "/dev/zero",
            "/dev/random",
            "/dev/urandom",
            "/dev/full",
            "/dev/stdin",
            "/dev/tty",
            "/dev/console",
            "/dev/stdout",
            "/dev/stderr",
            "/dev/fd/0",
            "/dev/fd/1",
            "/dev/fd/2",
        ] {
            assert!(is_device_file(p), "{p} should be a device file");
        }
        // /proc fd + the six special files.
        assert!(is_device_file("/proc/self/fd/1"));
        assert!(is_device_file("/proc/123/fd/2"));
        assert!(is_device_file("/proc/123/environ"));
        assert!(is_device_file("/proc/self/maps"));
        assert!(is_device_file("/proc/1/cmdline"));
        assert!(is_device_file("/proc/9/stat"));
        // NOT device files.
        assert!(!is_device_file("/dev/null")); // notably NOT in the blocked set
        assert!(!is_device_file("/home/user/file.txt"));
        assert!(!is_device_file("/proc/cpuinfo")); // no <pid> segment
        assert!(!is_device_file("/proc/123/foo/environ")); // too many segments
        assert!(!is_device_file("/proc/123/status")); // "status" != the "stat" name
        assert!(!is_device_file("/proc//environ")); // empty pid segment
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stable_leaf_symlink_is_read_through_approved_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&target, "approved").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);

        let result = tool
            .call(
                json!({ "file_path": link.to_string_lossy() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "approved");
    }

    #[test]
    fn symlink_resolution_changed_message_is_byte_exact() {
        assert_eq!(
            symlink_resolution_changed_message("/tmp/link.txt"),
            "Refusing to read /tmp/link.txt: its symlink resolution changed after permission was checked. If a link in the working directory is being rewritten concurrently, stop that and retry."
        );
    }

    fn make_ctx(tmp: &TempDir) -> (BuiltinToolContext, Arc<InMemorySink>) {
        let fs = make_dummy_fs();
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        (
            tool_api::test_support::ctx_for_file_tools(fs, bus, vec![tmp.path().to_path_buf()]),
            sink,
        )
    }

    #[test]
    fn max_size_byte_locked() {
        assert_eq!(MAX_FILE_READ_SIZE, 262_144);
    }

    #[tokio::test]
    async fn ranged_streaming_read_keeps_only_selected_lines() {
        let mut body = String::new();
        for i in 1..=5_000 {
            body.push_str(&format!("line-{i}\n"));
        }
        let result = read_text_range_streaming(
            std::io::Cursor::new(body.into_bytes()),
            Path::new("big.txt"),
            1,
            2,
            None,
            100_000,
        )
        .await
        .expect("stream");
        assert!(result.content.contains("line-1"), "got {}", result.content);
        assert!(result.read_lines <= 2);
        assert!(
            result.content.len() < 64,
            "selected body must not hold the whole file, got {}",
            result.content.len()
        );
        assert!(result.total_lines >= 5_000);
    }

    #[test]
    fn tool_name_is_read() {
        assert_eq!(TOOL_NAME, "Read");
    }

    // ── Fix #2: short dedup string (jbi) ──────────────────────────────────────

    #[test]
    fn file_unchanged_stub_is_byte_locked() {
        // `tld` — long dedup form (FileReadTool/prompt.ts:7, binary ~196574xxx).
        // The em-dash is U+2014.
        assert_eq!(
            FILE_UNCHANGED_STUB,
            "File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current \u{2014} refer to that instead of re-reading."
        );
    }

    #[test]
    fn file_unchanged_short_is_byte_locked() {
        // `jbi` — short dedup form (binary `Ybi(){return jbi}`, ~196575xxx).
        // The em-dash is U+2014 (Unicode character, not ASCII hyphen).
        assert_eq!(
            FILE_UNCHANGED_SHORT,
            "Wasted call \u{2014} file unchanged since your last Read. Refer to that earlier tool_result instead."
        );
        // Confirm the em-dash is present (guards against ASCII hyphen-dash).
        assert!(FILE_UNCHANGED_SHORT.contains('\u{2014}'));
        // Confirm it starts with "Wasted call" (matches `Ybi()` return value).
        assert!(FILE_UNCHANGED_SHORT.starts_with("Wasted call"));
    }

    #[test]
    fn is_dedup_result_detects_both_forms() {
        // `joo(e)` (@229139264) — checks
        //   startsWith(FOg) || startsWith(Sou) || startsWith(Tou).
        // The first two use the FULL constant as the prefix; `Tou` is itself a
        // proper prefix of the seeded stub built by `vou` (@229139106).
        assert!(is_dedup_result(FILE_UNCHANGED_STUB));
        assert!(is_dedup_result(FILE_UNCHANGED_SHORT));
        // A string that starts with the full long-form constant (e.g. with
        // trailing whitespace added by some wrapper).
        let long_with_suffix = format!("{FILE_UNCHANGED_STUB} extra");
        assert!(is_dedup_result(&long_with_suffix));
        // A string that starts with the full short-form constant.
        let short_with_suffix = format!("{FILE_UNCHANGED_SHORT} extra");
        assert!(is_dedup_result(&short_with_suffix));
        // Non-dedup strings do NOT match.
        assert!(!is_dedup_result(""));
        assert!(!is_dedup_result("File unchanged since")); // truncated prefix only
        assert!(!is_dedup_result("File has been modified since read"));
        assert!(!is_dedup_result("File does not exist."));
        assert!(!is_dedup_result("Wasted call")); // too short to match jbi
    }

    #[test]
    fn is_dedup_result_detects_the_seeded_prefix() {
        // `joo`'s third arm: `e.startsWith(Tou)` where
        // Tou = "<system-reminder>This file is already in your context" (@229141864).
        // `vou(path)` (@229139106) builds the full seeded stub by appending
        // ` (see "Contents of ${path}" above) and has not changed on disk. Use that
        // content instead of re-reading.</system-reminder>`.
        assert_eq!(
            FILE_UNCHANGED_SEEDED_PREFIX,
            "<system-reminder>This file is already in your context"
        );
        assert!(is_dedup_result(FILE_UNCHANGED_SEEDED_PREFIX));
        let seeded = format!(
            "{FILE_UNCHANGED_SEEDED_PREFIX} (see \"Contents of /repo/LINGXI.md\" above) and has not changed on disk. Use that content instead of re-reading.</system-reminder>"
        );
        assert!(is_dedup_result(&seeded));
        // A near-miss system-reminder must NOT match.
        assert!(!is_dedup_result("<system-reminder>This file is already"));
        assert!(!is_dedup_result(EMPTY_FILE_WARNING));
    }

    #[test]
    fn too_large_message_byte_locked() {
        // VERBATIM claude-code `FileTooLargeError` (`readFileInRange.ts:62-64`):
        // both sizes via `formatFileSize`; `path` is unused by the TS message.
        // 300_000 bytes => 292.969KB rounds to "293.0KB" => ".0" trimmed => "293KB".
        let msg = format_too_large(std::path::Path::new("/tmp/x"), 300_000);
        assert_eq!(
            msg,
            "File content (293KB) exceeds maximum allowed size (256KB). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file."
        );
        assert!(msg.contains("Use offset and limit"));
    }

    #[test]
    fn binary_message_byte_locked() {
        // VERBATIM claude-code `FileReadTool.ts:479` — the file's lowercased
        // extension is interpolated (`.bin` here).
        let msg = format_binary(std::path::Path::new("/tmp/x.bin"));
        assert_eq!(
            msg,
            "This tool cannot read binary files. The file appears to be a binary .bin file. Please use appropriate tools for binary file analysis."
        );
        // Extensionless file => empty extension (TS `path.extname` => "").
        let msg2 = format_binary(std::path::Path::new("/tmp/x"));
        assert_eq!(
            msg2,
            "This tool cannot read binary files. The file appears to be a binary  file. Please use appropriate tools for binary file analysis."
        );
    }

    #[test]
    fn max_lines_to_read_byte_locked() {
        // Prompt-text constant only (the "up to 2000 lines" wording); NOT a
        // runtime cap. The Read prompt below interpolates it.
        assert_eq!(MAX_LINES_TO_READ, 2000);
    }

    #[test]
    fn default_max_output_tokens_byte_locked() {
        let _env = READ_LIMIT_ENV_MUTEX.blocking_lock();
        std::env::remove_var(MAX_OUTPUT_TOKENS_ENV);
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 25_000);
        assert_eq!(effective_max_output_tokens(), DEFAULT_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn max_output_tokens_env_override_is_per_call() {
        let _env = READ_LIMIT_ENV_MUTEX.blocking_lock();
        std::env::set_var(MAX_OUTPUT_TOKENS_ENV, "8");
        assert_eq!(effective_max_output_tokens(), 8);

        std::env::set_var(MAX_OUTPUT_TOKENS_ENV, "13");
        assert_eq!(effective_max_output_tokens(), 13);

        std::env::set_var(MAX_OUTPUT_TOKENS_ENV, "not-a-number");
        assert_eq!(effective_max_output_tokens(), DEFAULT_MAX_OUTPUT_TOKENS);

        std::env::remove_var(MAX_OUTPUT_TOKENS_ENV);
    }

    #[test]
    fn has_binary_extension_matches_ts_set() {
        use std::path::Path;
        // A representative spread across the BINARY_EXTENSIONS categories.
        assert!(has_binary_extension(Path::new("/a/b.bin")));
        assert!(has_binary_extension(Path::new("/a/b.EXE"))); // case-insensitive
        assert!(has_binary_extension(Path::new("/a/archive.tar.gz")));
        assert!(has_binary_extension(Path::new("/a/font.woff2")));
        assert!(has_binary_extension(Path::new("/a/lib.rlib")));
        assert!(has_binary_extension(Path::new("/a/db.sqlite3")));
        // Image + PDF extensions ARE in the set (excluded at the call site, not
        // here — `has_binary_extension` mirrors TS `hasBinaryExtension` exactly).
        assert!(has_binary_extension(Path::new("/a/i.png")));
        assert!(has_binary_extension(Path::new("/a/d.pdf")));
        // Text / source extensions are NOT binary.
        assert!(!has_binary_extension(Path::new("/a/main.rs")));
        assert!(!has_binary_extension(Path::new("/a/readme.md")));
        assert!(!has_binary_extension(Path::new("/a/data.json")));
        // No extension => whole name lowercased, never in the set.
        assert!(!has_binary_extension(Path::new("/a/Makefile")));
    }

    #[tokio::test]
    async fn happy_path_reads_full_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "hello\nworld\n").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "hello\nworld\n");
        // FILE.4: TS counts the trailing newline's empty final fragment as a line
        // ("hello\nworld\n" => 3, matching readFileInRange `lineIndex`).
        assert_eq!(result.data["file"]["totalLines"], 3);
        // Model-facing string: cat -n (compact tab format, 1-based from offset).
        // No cyber-risk reminder (parity verdict 12/14 — removed).
        assert_eq!(
            result.model_content.as_deref().unwrap(),
            "1\thello\n2\tworld\n3\t"
        );
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_read_started"));
        assert!(names.contains(&"tengu_tool_read_completed"));
    }

    /// Worktree parity plan (Task 3): a RELATIVE `file_path` must resolve
    /// against the CURRENT `session_cwd` — so after `EnterWorktree` swaps the
    /// cwd into a worktree, a relative-path `Read` lands under the worktree,
    /// NOT the boot cwd (which is what `std::fs::canonicalize` would
    /// otherwise resolve a relative path against, since it consults the
    /// frozen OS process cwd, not the switchable session cwd).
    #[tokio::test]
    async fn relative_path_read_follows_session_cwd_swap_into_worktree() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let target = wt.join("rel.txt");
        std::fs::write(&target, "in the worktree\n").unwrap();
        // A DIFFERENT file with the same relative name at the boot cwd root —
        // proves the read comes from the worktree, not this one.
        std::fs::write(tmp.path().join("rel.txt"), "at the boot cwd\n").unwrap();

        let (ctx, _sink) = make_ctx(&tmp);
        // Swap the session cwd into the worktree subdir (trusting it too),
        // the way `EnterWorktree` does.
        ctx.session_cwd.swap(wt.clone(), vec![wt.clone()]);
        assert_eq!(ctx.cwd(), wt);

        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(json!({ "file_path": "rel.txt" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        assert_eq!(
            result.data["file"]["content"], "in the worktree\n",
            "relative file_path must resolve under the SWAPPED worktree cwd"
        );
    }

    /// INERT INVARIANT companion: with no `session_cwd.swap(..)` call, a
    /// relative `file_path` resolves under the boot cwd exactly as it did
    /// before this task's fix.
    #[tokio::test]
    async fn relative_path_read_resolves_under_boot_cwd_without_swap() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("rel.txt"), "at the boot cwd\n").unwrap();

        let (ctx, _sink) = make_ctx(&tmp);
        assert_eq!(ctx.cwd(), tmp.path(), "no swap happened in this test");

        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(json!({ "file_path": "rel.txt" }), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "at the boot cwd\n");
    }

    #[tokio::test]
    async fn rejects_oversize_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("big.txt");
        std::fs::write(&target, vec![b'A'; (MAX_FILE_READ_SIZE + 1) as usize]).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exceeds maximum allowed size"), "got: {msg}");
        assert!(msg.contains("Use offset and limit"), "got: {msg}");
    }

    #[tokio::test]
    async fn ranged_read_of_oversize_file_returns_requested_lines() {
        // FILE.1: TS gates the 256KB cap on `limit === undefined`
        // (FileReadTool.ts:1026). A >256KB file read with offset+limit must
        // return just the requested range, NOT the too-large error.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("big_ranged.txt");
        // Build a >256KB file made of distinct 1-based lines so we can assert
        // the slice precisely. Each "lineNN\n" is small; pad to exceed the cap.
        let mut body = String::from("first\nsecond\nthird\n");
        // Fill past MAX_FILE_READ_SIZE with filler lines.
        while body.len() as u64 <= MAX_FILE_READ_SIZE {
            body.push_str("filler-line-of-some-length\n");
        }
        assert!(body.len() as u64 > MAX_FILE_READ_SIZE);
        std::fs::write(&target, &body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 2 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ranged read of an oversize file must succeed");
        assert_eq!(result.data["file"]["content"], "second\nthird\n");
    }

    #[tokio::test]
    async fn ranged_read_of_oversize_single_line_returns_bounded_error() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("huge_single_line.txt");
        std::fs::write(&target, "x".repeat((MAX_FILE_READ_SIZE as usize) + 1024)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 1, "limit": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("oversize single-line range must fail without loading the whole line");
        let msg = err.to_string();
        assert!(msg.contains("line longer than"), "got: {msg}");
        assert!(msg.contains("Use Grep"), "got: {msg}");
    }

    #[tokio::test]
    async fn no_limit_read_of_oversize_file_still_errors() {
        // FILE.1 invariant: with no `limit`, the byte cap still applies and the
        // byte-locked too-large error is returned (fixture-pinned template).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("big_nolimit.txt");
        let mut body = String::from("first\nsecond\nthird\n");
        while body.len() as u64 <= MAX_FILE_READ_SIZE {
            body.push_str("filler-line-of-some-length\n");
        }
        std::fs::write(&target, &body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exceeds maximum allowed size"), "got: {msg}");
        assert!(msg.contains("Use offset and limit"), "got: {msg}");
    }

    #[tokio::test]
    async fn rejects_binary_file() {
        // Extensionless file with a NUL byte in the first 8 KB => NUL-scan
        // fallback path (no binary extension to short-circuit on). The TS message
        // renders the (empty) extension.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bin");
        let mut data = vec![b'A'; 100];
        data[10] = 0;
        std::fs::write(&target, &data).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("This tool cannot read binary files."));
    }

    #[tokio::test]
    async fn rejects_binary_extension_file_before_read() {
        // A `.bin` file routes to the binary-extension gate (`FileReadTool.ts:
        // 469-482`) BEFORE any byte read — even when its bytes are pure ASCII
        // (no NUL). The TS message interpolates the `.bin` extension.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("payload.bin");
        std::fs::write(&target, b"this is plain ascii, no NUL bytes").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // `ToolError::Io`'s Display prepends "io: "; assert the message tail.
        assert!(
            err.to_string().ends_with(
                "This tool cannot read binary files. The file appears to be a binary .bin file. Please use appropriate tools for binary file analysis."
            ),
            "got: {err}"
        );
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_read_failed"));
    }

    #[tokio::test]
    async fn no_limit_read_returns_all_lines_within_budget_no_note() {
        // Depends on the DEFAULT token budget; a sibling test mutates the env
        // override, so take the same lock it does.
        let _env = READ_LIMIT_ENV_MUTEX.lock().await;
        // There is NO 2000-line cap. A no-`limit` read of a file with > 2000
        // (but small / within-token-budget) lines returns ALL lines, with NO
        // truncation note (claude-code passes `maxLines = limit` verbatim, and
        // `undefined` reads to EOF).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("long.txt");
        let mut body = String::new();
        for i in 1..=2500u32 {
            body.push_str(&format!("L{i}\n"));
        }
        assert!(
            (body.len() as u64) < MAX_FILE_READ_SIZE,
            "must stay under byte cap"
        );
        // ~14 KB / 4 ≈ 3500 tokens, well under the 25000 token budget.
        assert!(
            rough_token_count_estimation_for_file_type(&body, Some("txt"))
                < DEFAULT_MAX_OUTPUT_TOKENS,
            "fixture must stay under the token budget so it reads in full"
        );
        std::fs::write(&target, &body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("no-limit read must succeed");
        // ALL 2500 lines returned (no cap).
        assert_eq!(
            result.data["file"]["content"]
                .as_str()
                .unwrap()
                .lines()
                .count(),
            2500
        );
        assert!(result.data["file"]["content"]
            .as_str()
            .unwrap()
            .ends_with("L2500\n"));
        // startLine = 1, numLines = 2500 (lines returned); totalLines = 2501
        // (trailing-newline phantom line).
        assert_eq!(result.data["file"]["startLine"], 1);
        assert_eq!(result.data["file"]["numLines"], 2500);
        assert_eq!(result.data["file"]["totalLines"], 2501);
        // NO truncation note of any kind.
        let mc = result.model_content.as_deref().unwrap();
        assert!(
            !mc.contains("Truncated") && !mc.contains("File truncated"),
            "within-budget full read must not surface any truncation note, got tail: {}",
            &mc[mc.len().saturating_sub(200)..]
        );
    }

    #[tokio::test]
    async fn explicit_limit_over_2000_is_honored_without_note() {
        // An explicit `limit` is the caller's choice and is honored verbatim
        // (no 2000-line cap was ever applied), and gets no truncation note even
        // when it elides trailing lines.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("long2.txt");
        let mut body = String::new();
        for i in 1..=2500u32 {
            body.push_str(&format!("L{i}\n"));
        }
        std::fs::write(&target, &body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "limit": 2200 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("explicit-limit read must succeed");
        // 2200 lines returned (> 2000 — not clamped).
        assert_eq!(
            result.data["file"]["content"]
                .as_str()
                .unwrap()
                .lines()
                .count(),
            2200
        );
        let mc = result.model_content.as_deref().unwrap();
        assert!(
            !mc.contains("Truncated") && !mc.contains("File truncated"),
            "explicit limit must not emit a truncation note"
        );
    }

    #[tokio::test]
    async fn verbatim_description_and_prompt() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let desc = tool
            .description(
                &json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert_eq!(desc, "Read a file from the local filesystem.");
        let prompt = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: None,
                model_profile: None,
            })
            .await;
        // model:None ⇒ Dh(undefined)=false ⇒ LONG prompt.
        assert_eq!(prompt, READ_PROMPT_LONG);
        // Spot-check the VERBATIM template fragments.
        assert!(prompt.starts_with(
            "Reads a file from the local filesystem. You can access any file directly by using this tool."
        ));
        assert!(prompt.contains(
            "By default, it reads up to 2000 lines starting from the beginning of the file\n"
        ));
        assert!(prompt.contains(
            "- Results are returned using cat -n format, with line numbers starting at 1"
        ));
        assert!(prompt
            .contains("it's recommended to read the whole file by not providing these parameters"));
        // PDF fragment is INCLUDED in the default (PDF-supported) build.
        assert!(prompt.contains("This tool can read PDF files (.pdf)."));
        // R-T3: directory line uses the registered-shell-tool wording.
        assert!(prompt
            .contains("This tool can only read files, not directories. To list files in a directory, use the registered shell tool."));
        assert!(!prompt.contains("Bash"));
        // R-T3: the qhi final bullet is present (em-dash U+2014).
        assert!(prompt.ends_with(
            "- Do NOT re-read a file you just edited to verify \u{2014} Edit/Write would have errored if the change failed, and the harness tracks file state for you."
        ));
    }

    #[tokio::test]
    async fn short_prompt_for_simple_system_model() {
        let tmp = TempDir::new().unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        // model:claude-opus-4-8 ⇒ Dh=true ⇒ SHORT prompt (byte-anchor).
        let prompt = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: Some("claude-opus-4-8".to_string()),
                model_profile: None,
            })
            .await;
        assert_eq!(prompt, READ_PROMPT_SHORT);
        assert!(prompt.starts_with(
            "Reads a file from the local filesystem.\n\n- `file_path` must be an absolute path."
        ));
        assert!(prompt.contains("- Reads up to 2000 lines by default.\n"));
        assert!(prompt.contains("Reads PDFs via the `pages` parameter (e.g. \"1-5\", max 20 pages/request; required for PDFs over 10 pages)."));
        assert!(prompt.ends_with(
            "- Do NOT re-read a file you just edited to verify \u{2014} Edit/Write would have errored if the change failed, and the harness tracks file state for you."
        ));
    }

    #[tokio::test]
    async fn offset_1_returns_from_first_line() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "line1\nline2\nline3\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 1, "limit": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "line1\n");
        // cat -n numbers from offset=1. No cyber-risk reminder (verdict 12/14).
        assert_eq!(result.model_content.as_deref().unwrap(), "1\tline1\n2\t");
    }

    #[tokio::test]
    async fn offset_2_skips_one_line() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "line1\nline2\nline3\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "line2\n");
        // Numbering starts at the requested offset (2), not 1. No cyber-risk
        // reminder (verdict 12/14).
        assert_eq!(result.model_content.as_deref().unwrap(), "2\tline2\n3\t");
    }

    #[tokio::test]
    async fn limit_greater_than_remaining_returns_what_exists() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "line1\nline2\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 50 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "line2\n");
    }

    #[tokio::test]
    async fn no_offset_no_limit_returns_full_content() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "alpha\nbeta\ngamma\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "alpha\nbeta\ngamma\n");
        // FILE.4: trailing newline => phantom final line ("...\n" => 4).
        assert_eq!(result.data["file"]["totalLines"], 4);
    }

    #[tokio::test]
    async fn read_populates_read_file_state_map_with_offset_limit() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("a.txt");
        std::fs::write(&target, "line1\nline2\nline3\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        // Hold a handle to the shared registry BEFORE the ctx is moved into the
        // tool — the tool's `readFileState.set` mutates this same `Arc`.
        let map = ctx.read_file_state.clone();
        let tool = FileReadTool::new(ctx);
        // canonicalize the target the same way the tool keys the entry.
        let canon = std::fs::canonicalize(&target).unwrap();
        tool.call(
            json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 1 }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let entry =
            tool_api::read_file_state::get(&map, &canon).expect("registry entry recorded on read");
        // Content is the range-limited slice (TS stores `readFileInRange`'s
        // output), and offset/limit are the verbatim input values.
        assert_eq!(entry.content, "line2\n");
        assert_eq!(entry.offset, Some(2));
        assert_eq!(entry.limit, Some(1));
        // mtime recorded as a non-negative floor-truncated millisecond value.
        assert!(entry.mtime_ms >= 0);
    }

    #[tokio::test]
    async fn read_without_offset_limit_records_none() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("b.txt");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let map = ctx.read_file_state.clone();
        let tool = FileReadTool::new(ctx);
        let canon = std::fs::canonicalize(&target).unwrap();
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let entry = tool_api::read_file_state::get(&map, &canon).unwrap();
        assert_eq!(entry.content, "alpha\nbeta\n");
        assert_eq!(entry.offset, None);
        assert_eq!(entry.limit, None);
    }

    #[tokio::test]
    async fn failed_read_does_not_populate_map() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("missing.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let map = ctx.read_file_state.clone();
        let tool = FileReadTool::new(ctx);
        let _ = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        // A read that errored (missing file) records nothing.
        assert!(map.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn utf8_bom_stripped_on_read() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bom.txt");
        // BOM + "hello"
        let mut content = vec![0xEF, 0xBB, 0xBF];
        content.extend_from_slice(b"hello");
        std::fs::write(&target, &content).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "hello");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_path_outside_trusted() {
        // Deviation from plan: std::fs::canonicalize requires every path
        // component to exist, so a bare outside path errors via Io rather
        // than Outside. Use a symlink inside the trusted tempdir pointing
        // at an outside location — canonicalize follows the symlink and
        // produces a real "Outside" rejection.
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target_real = outside.path().join("a.txt");
        std::fs::write(&target_real, "x").unwrap();
        let link = tmp.path().join("escape");
        std::os::unix::fs::symlink(&target_real, &link).unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": link.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::PathBlocked { .. } => {}
            other => panic!("expected PathBlocked, got {other:?}"),
        }
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_file_path_blocked"));
        assert!(names.contains(&"tengu_tool_read_failed"));
    }

    #[tokio::test]
    async fn accepts_path_inside_additional_trusted_dir() {
        // parity 2.1.207 P1-08: file tools accept a path inside an `--add-dir` /
        // settings `additionalDirectories` root (trusted_dirs = [cwd, extra]),
        // not just cwd. claude-code allows file tools in additionalWorkingDirectories.
        let cwd = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let target = extra.path().join("note.txt");
        std::fs::write(&target, "hello from extra").unwrap();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![cwd.path().to_path_buf(), extra.path().to_path_buf()],
        );
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "hello from extra");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_path_outside_all_trusted_dirs() {
        // With two trusted dirs [cwd, extra], a path outside BOTH is still
        // blocked. Symlink inside `extra` pointing out → canonicalize escapes.
        let cwd = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target_real = outside.path().join("secret.txt");
        std::fs::write(&target_real, "x").unwrap();
        let link = extra.path().join("escape");
        std::os::unix::fs::symlink(&target_real, &link).unwrap();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![cwd.path().to_path_buf(), extra.path().to_path_buf()],
        );
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": link.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PathBlocked { .. }),
            "expected PathBlocked, got {err:?}"
        );
    }

    /// S2 (PathAtlas): a guest path maps onto its host twin BEFORE
    /// canonicalization, so the read serves the host file's bytes. The alias
    /// fs panics on every I/O method — passing also proves the tool still
    /// reads through raw `tokio::fs`.
    #[tokio::test]
    async fn guest_path_reads_through_its_host_twin() {
        let host = TempDir::new().unwrap();
        std::fs::write(host.path().join("note.txt"), "guest sees host bytes").unwrap();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_guest_alias_fs("/workspace/abc", host.path(), "/fenced"),
            Arc::new(AnalyticsBus::new()),
            vec![host.path().to_path_buf()],
        );
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": "/workspace/abc/note.txt" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "guest sees host bytes");
    }

    /// S2 (PathAtlas): guest space the filesystem refuses to host-back
    /// surfaces the fs's own fence message as `InvalidInput`.
    #[tokio::test]
    async fn fenced_guest_path_is_refused_with_the_fs_message() {
        let host = TempDir::new().unwrap();
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_guest_alias_fs("/workspace/abc", host.path(), "/fenced"),
            Arc::new(AnalyticsBus::new()),
            vec![host.path().to_path_buf()],
        );
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": "/fenced/secret.txt" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(message) => {
                assert!(message.contains("not host-backed"), "{message}");
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_file_emits_empty_warning_model_content() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("empty.txt");
        std::fs::write(&target, "").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // TUI payload `content` stays the raw (empty) slice; the model sees the
        // empty-file warning instead of an empty string.
        assert_eq!(result.data["file"]["content"], "");
        assert_eq!(result.data["file"]["totalLines"], 0);
        assert_eq!(result.model_content.as_deref().unwrap(), EMPTY_FILE_WARNING);
    }

    #[tokio::test]
    async fn offset_beyond_eof_emits_offset_warning_model_content() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("short.txt");
        std::fs::write(&target, "a\nb\nc\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 10 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "");
        // FILE.4: "a\nb\nc\n" has 3 newlines => TS total_lines 4 (phantom final line).
        assert_eq!(result.data["file"]["totalLines"], 4);
        assert_eq!(
            result.model_content.as_deref().unwrap(),
            "<system-reminder>Warning: the file exists but is shorter than the provided offset (10). The file has 4 lines.</system-reminder>"
        );
    }

    #[test]
    fn add_line_numbers_compact_format() {
        assert_eq!(add_line_numbers("", 1), "");
        assert_eq!(
            add_line_numbers("hello\nworld\n", 1),
            "1\thello\n2\tworld\n3\t"
        );
        // Numbering starts at `start_line`.
        assert_eq!(add_line_numbers("x", 5), "5\tx");
        // CRLF: a trailing \r is stripped per line (mirrors the TS /\r?\n/ split).
        assert_eq!(add_line_numbers("a\r\nb", 1), "1\ta\n2\tb");
    }

    // (removed) `mitigation_reminder_gated_on_model` — the per-read cyber-risk
    // reminder does not exist in claude-code v2.1.183 (parity verdict 12/14); the
    // defensive-security guidance lives in the system-prompt body instead.

    // ───────────────────────── Notebook (.ipynb) structured read ────────────

    fn sample_ipynb() -> String {
        serde_json::to_string(&json!({
            "cells": [
                {
                    "cell_type": "code",
                    "id": "c1",
                    "source": ["print(", "'hi')"],
                    "execution_count": 2,
                    "outputs": [
                        { "output_type": "stream", "name": "stdout", "text": "hi\n" }
                    ]
                },
                { "cell_type": "markdown", "id": "c2", "source": "# Title" }
            ],
            "metadata": { "language_info": { "name": "python" } },
            "nbformat": 4,
            "nbformat_minor": 5
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn ipynb_read_returns_structured_cells() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_ipynb()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Notebook result variant: structured `cells`, NOT line-numbered text.
        assert_eq!(result.data["type"], "notebook");
        let cells = result.data["file"]["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 2);
        // Code cell: camelCase cellType, joined source, execution_count, language.
        assert_eq!(cells[0]["cellType"], "code");
        assert_eq!(cells[0]["source"], "print('hi')");
        assert_eq!(cells[0]["execution_count"], 2);
        assert_eq!(cells[0]["cell_id"], "c1");
        assert_eq!(cells[0]["language"], "python");
        assert_eq!(cells[0]["outputs"][0]["output_type"], "stream");
        assert_eq!(cells[0]["outputs"][0]["text"], "hi\n");
        // Markdown cell: cellType markdown, no language/outputs.
        assert_eq!(cells[1]["cellType"], "markdown");
        assert!(cells[1].get("language").is_none());
        // The model-facing string is the cell-block text projection.
        assert_eq!(
            result.model_content.as_deref().unwrap(),
            "<cell id=\"c1\">print('hi')</cell id=\"c1\">\n\nhi\n\n<cell id=\"c2\"><cell_type>markdown</cell_type># Title</cell id=\"c2\">"
        );
    }

    #[tokio::test]
    async fn ipynb_read_records_full_view_registry_entry() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nb.ipynb");
        std::fs::write(&target, sample_ipynb()).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let map = ctx.read_file_state.clone();
        let tool = FileReadTool::new(ctx);
        let canon = std::fs::canonicalize(&target).unwrap();
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let entry = tool_api::read_file_state::get(&map, &canon).expect("notebook read recorded");
        // A no-range notebook read is a full view (offset/limit None) and is
        // flagged as Read-sourced so the staleness guard accepts a follow-up
        // NotebookEdit and the dedup gate sees a Read entry.
        assert_eq!(entry.offset, None);
        assert_eq!(entry.limit, None);
        assert!(entry.from_read);
        // Stored content is the serialized cells JSON (so a same-range re-read
        // dedups byte-for-byte).
        assert!(entry.content.contains("\"cellType\""));
    }

    #[tokio::test]
    async fn ipynb_invalid_json_errors() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bad.ipynb");
        std::fs::write(&target, "not a notebook").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Notebook file is not valid JSON (it may be truncated, corrupted, or still being written):"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn ipynb_cell_without_source_is_refused() {
        // FT-02: the oracle's `X2t` per-cell gate — `cells:[{...}]` with no
        // `source` is NOT a readable notebook. Before FT-02 LingXi accepted it
        // and rendered an empty cell.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nosource.ipynb");
        std::fs::write(
            &target,
            r#"{"cells":[{"cell_type":"code","id":"c1"}],"metadata":{}}"#,
        )
        .unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Io(m) => {
                assert_eq!(m, crate::notebook_read::INVALID_NOTEBOOK_SHAPE);
            }
            other => panic!("expected Io with the invalid-shape message, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ipynb_fifo_is_refused_as_not_a_regular_file() {
        // FT-04: `K0f`'s `if(!n.isFile()&&!n.isDirectory())` guard. A FIFO is
        // neither, so the notebook branch refuses before reading a byte.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("fifo.ipynb");
        let made = std::process::Command::new("mkfifo")
            .arg(&target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            // No `mkfifo` on this host — nothing to assert.
            return;
        }
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Io(m) => {
                assert_eq!(m, crate::notebook_read::NOTEBOOK_NOT_REGULAR_FILE);
            }
            other => panic!("expected Io with the not-a-regular-file message, got {other:?}"),
        }
    }

    // ───────────────────────── Read dedup (file_unchanged) ──────────────────

    #[tokio::test]
    async fn unchanged_reread_returns_file_unchanged_stub() {
        // A full Read, then an immediate identical re-read with no on-disk
        // change → the dedup short-circuit returns the byte-locked stub.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dedup.txt");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        // First read populates the registry.
        let first = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(first.data["file"]["content"], "alpha\nbeta\n");
        // Second identical read → file_unchanged stub.
        let second = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(second.data["type"], "file_unchanged");
        // file_unchanged data = {type, file:{filePath}}; the stub rides on the
        // model_content channel, not inside data.
        assert!(second.data.get("content").is_none());
        assert!(second.data["file"]["filePath"].is_string());
        assert_eq!(second.model_content.as_deref(), Some(FILE_UNCHANGED_SHORT));
    }

    #[tokio::test]
    async fn dedup_returns_the_short_form_wasted_call_stub() {
        // 2.1.220 result renderer (binary @235742950):
        //   case"file_unchanged": content = e.source==="seeded" ? vou(...) : Eou()
        // and `function Eou(){return Sou}` (@229139080), where
        //   Sou = "Wasted call — file unchanged since your last Read. …" (@229141759).
        // The long form `FOg` (@229141593 = FILE_UNCHANGED_STUB) has NO PRODUCER in
        // 2.1.220: `LC_ALL=C grep -abo -F 'FOg'` over the binary yields exactly two
        // JS-region hits — its definition (@229141593) and the detector
        // `joo(e){return e.startsWith(FOg)||e.startsWith(Sou)||e.startsWith(Tou)}`
        // (@229139264/@229139291). It survives only to recognise stubs written by
        // older versions, so the non-seeded dedup branch must emit `Sou`.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("short-stub.txt");
        std::fs::write(&target, "one\ntwo\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let second = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(second.data["type"], "file_unchanged");
        assert_eq!(
            second.model_content.as_deref(),
            Some(
                "Wasted call \u{2014} file unchanged since your last Read. Refer to that earlier tool_result instead."
            )
        );
    }

    #[tokio::test]
    async fn changed_file_reread_does_not_dedup() {
        use filetime::{set_file_mtime, FileTime};
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("changed.txt");
        std::fs::write(&target, "v0\n").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        // External modification bumps mtime → re-read must NOT dedup.
        std::fs::write(&target, "v1\n").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(2_000_000_000, 0)).unwrap();
        let again = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Fresh content returned, not the stub.
        assert_eq!(again.data["file"]["content"], "v1\n");
        assert_eq!(
            again.data["type"], "text",
            "fresh read is a text result, not a dedup"
        );
    }

    #[tokio::test]
    async fn different_range_reread_does_not_dedup() {
        // First read full, then a ranged re-read of the same (unchanged) file:
        // the range differs, so dedup must NOT fire.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("range.txt");
        std::fs::write(&target, "l1\nl2\nl3\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let ranged = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Different range → real content, no stub.
        assert_eq!(ranged.data["file"]["content"], "l2\n");
        assert_eq!(
            ranged.data["type"], "text",
            "fresh read is a text result, not a dedup"
        );
    }

    #[tokio::test]
    async fn write_then_read_does_not_dedup_against_post_write_entry() {
        // A Write records a post-write registry entry with `from_read=false`.
        // A subsequent Read of the same path must NOT dedup against it (TS gates
        // dedup on `offset !== undefined`, i.e. Read-sourced entries only).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("wr.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let map = ctx.read_file_state.clone();
        let canon = {
            std::fs::write(&target, "seed\n").unwrap();
            std::fs::canonicalize(&target).unwrap()
        };
        let mtime_ms = std::fs::metadata(&canon)
            .ok()
            .and_then(|m| m.modified().ok())
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        // Simulate a Write's post-write entry: full view, but NOT from a Read.
        tool_api::read_file_state::set(
            &map,
            canon,
            tool_api::read_file_state::ReadFileEntry {
                content: "seed\n".into(),
                mtime_ms,
                offset: None,
                limit: None,
                from_read: false,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Real content returned (the write entry is not a dedup candidate).
        assert_eq!(result.data["file"]["content"], "seed\n");
        assert_eq!(
            result.data["type"], "text",
            "fresh read is a text result, not a dedup"
        );
    }

    // ───────────────────────── Token-budget gate ────────────────────────────

    #[test]
    fn rough_token_estimate_byte_locked() {
        // Round-half-up over byte length, bpt=4 for non-json: (len+2)/4.
        assert_eq!(rough_token_count_estimation_for_file_type("", None), 0);
        assert_eq!(rough_token_count_estimation_for_file_type("ab", None), 1); // (2+2)/4
        assert_eq!(
            rough_token_count_estimation_for_file_type(&"a".repeat(100_000), None),
            25_000
        );
        // json/jsonl/jsonc use bpt=2 (denser tokens): (len+1)/2.
        assert_eq!(bytes_per_token_for_file_type(Some("json")), 2);
        assert_eq!(bytes_per_token_for_file_type(Some("jsonl")), 2);
        assert_eq!(bytes_per_token_for_file_type(Some("jsonc")), 2);
        assert_eq!(bytes_per_token_for_file_type(Some("txt")), 4);
        assert_eq!(bytes_per_token_for_file_type(None), 4);
        assert_eq!(
            rough_token_count_estimation_for_file_type(&"a".repeat(50_000), Some("json")),
            25_000
        );
    }

    #[test]
    fn max_tokens_message_byte_locked() {
        assert_eq!(
            format_max_tokens_exceeded(30_000, 25_000),
            "File content (30000 tokens) exceeds maximum allowed tokens (25000). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file."
        );
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 25_000);
    }

    #[test]
    fn validate_content_tokens_gate_behavior() {
        // Below maxTokens/4 (6250) → early pass.
        assert!(validate_content_tokens(&"a".repeat(1_000), None, 25_000).is_ok());
        // In the (maxTokens/4, maxTokens] band → passes (offline API fallback
        // admits it: effectiveCount == estimate <= maxTokens).
        // 80_000 bytes / 4 = 20_000 tokens (> 6250, <= 25000).
        assert!(validate_content_tokens(&"a".repeat(80_000), None, 25_000).is_ok());
        // Above maxTokens → error. 100_004 bytes / 4 = 25_001 > 25_000.
        assert!(validate_content_tokens(&"a".repeat(100_004), None, 25_000).is_err());
        // json density: 50_002 bytes / 2 = 25_001 > 25_000 → error.
        assert!(validate_content_tokens(&"a".repeat(50_002), Some("json"), 25_000).is_err());
    }

    /// A token-truncated read must record `is_partial_view: true`.
    ///
    /// Oracle @235732534 writes the entry as
    /// `p.set(r,{content:A,timestamp,offset:i,limit:D,...x!==void 0&&{isPartialView:!0}})`,
    /// and `x` is the truncation NOTE — assigned only inside
    /// `catch(U){if(U instanceof ZVe && O){...}}`, i.e. the token-cap overflow on
    /// a full-file read. So `isPartialView` is exactly "this read was
    /// token-truncated".
    ///
    /// It matters because `Aze` (@232453039) returns false for a partial view,
    /// and the Edit/Write guards (`if(!p||p.isPartialView)`, @232481342 /
    /// @232489643) then refuse to treat the file as read. Recording `false` here
    /// let a truncated read masquerade as a full one.
    #[tokio::test]
    async fn token_truncated_read_is_recorded_as_a_partial_view() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dense.txt");
        let mut body = String::new();
        for i in 0..14_000u32 {
            body.push_str(&format!("line {i:05}\n"));
        }
        std::fs::write(&target, &body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let map = Arc::clone(&ctx.read_file_state);
        let tool = FileReadTool::new(ctx);
        let r = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("truncating read still succeeds");
        // FT-08: the banner left `model_content` and became its own isMeta
        // notice, so the truncation branch is now witnessed through
        // `new_messages` (this assertion used to read `model_content`).
        assert_eq!(
            r.new_messages.len(),
            1,
            "fixture must actually trigger the truncation branch"
        );

        let canon = std::fs::canonicalize(&target).unwrap();
        let entry =
            tool_api::read_file_state::get(&map, &canon).expect("the read recorded an entry");
        assert!(
            entry.is_partial_view,
            "a token-truncated read is a PARTIAL view, not a full read"
        );
    }

    #[tokio::test]
    async fn full_read_over_token_budget_gracefully_truncates() {
        let _env = READ_LIMIT_ENV_MUTEX.lock().await;
        // A FULL read (no offset, no limit) whose estimated tokens exceed
        // DEFAULT_MAX_OUTPUT_TOKENS (25000) but stay under the 256KB byte cap is
        // GRACEFULLY token-truncated (claude-code's `if(L instanceof Fae && k)`
        // catch branch) — it SUCCEEDS, surfaces the `[Truncated: PARTIAL view —
        // showing lines 1-N of H total ...]` note, and emits read_COMPLETED.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dense.txt");
        // A file over the 25000-token budget (>100 KB at bpt=4) but under the
        // 256 KB byte cap. ~14000 lines of "line NNNNN\n" (~10 bytes each) ≈
        // 140 KB → ~35000 tokens.
        let mut body = String::new();
        for i in 0..14_000u32 {
            body.push_str(&format!("line {i:05}\n"));
        }
        // Byte length / 4 > 25000 tokens but < 256 KB.
        assert!(
            (body.len() as u64) < MAX_FILE_READ_SIZE,
            "must stay under byte cap"
        );
        assert!(
            rough_token_count_estimation_for_file_type(&body, Some("txt"))
                > DEFAULT_MAX_OUTPUT_TOKENS,
            "fixture must exceed the token budget"
        );
        std::fs::write(&target, &body).unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("over-budget full read must gracefully truncate, not error");
        let mc = result.model_content.as_deref().unwrap();
        let canon = std::fs::canonicalize(&target).unwrap();
        // FT-08 (delivery): the banner is NOT inside the tool_result. Upstream
        // `Sgm` returns `{data:L}` and stashes the banner via `daf(L,R)`; the
        // dispatcher republishes it as a `read_truncation_notice` attachment.
        // This assertion is the inverse of the one that stood here before —
        // that test pinned the concatenation this finding removes.
        assert!(
            !mc.contains("[Truncated: PARTIAL view"),
            "the partial-view banner must NOT be concatenated onto the tool_result, tail: {}",
            &mc[mc.len().saturating_sub(400)..]
        );
        // …it arrives as its own isMeta user message instead.
        assert_eq!(
            result.new_messages.len(),
            1,
            "a token-truncated read must inject exactly one truncation notice"
        );
        let banner = match &result.new_messages[0] {
            lingxi_core::types::ConversationMessage::User {
                content, is_meta, ..
            } => {
                assert!(*is_meta, "the truncation notice is an isMeta message");
                content
                    .iter()
                    .find_map(|b| match b {
                        lingxi_core::types::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .expect("truncation notice text block")
            }
            other => panic!("truncation notice must be a user message, got {other:?}"),
        };
        // Exact note shape (line-paging branch): prefix + em-dash +
        // fullFilePath + `: ` + range (FT-08 — `${r}: showing lines 1-…`).
        assert!(
            banner.starts_with(&format!(
                "[Truncated: PARTIAL view \u{2014} {}: showing lines 1-",
                canon.display()
            )),
            "the notice must carry the partial-view banner WITH the full path: {banner}"
        );
        assert!(banner.contains(" total ("));
        assert!(banner.contains(" tokens, cap 25000). Call Read with offset="));
        assert!(banner.contains(" or Grep to find a specific section."));
        assert!(banner.contains("Do NOT answer from this page alone"));
        // The returned slice is strictly smaller than the whole file.
        let returned_lines = result.data["file"]["content"]
            .as_str()
            .unwrap()
            .lines()
            .count();
        assert!(
            returned_lines < 14_000,
            "must be truncated, got {returned_lines}"
        );
        // Emits read_COMPLETED (graceful path), not read_failed.
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_read_completed"));
        assert!(!names.contains(&"tengu_tool_read_failed"));
    }

    #[tokio::test]
    async fn explicit_range_over_token_budget_errors() {
        let _env = READ_LIMIT_ENV_MUTEX.lock().await;
        // An EXPLICIT-range read (limit set, or offset > 1) over the token
        // budget RE-THROWS as a hard error (`else throw L`) — it is NOT a full
        // read, so the graceful path does not apply.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dense2.txt");
        let mut body = String::new();
        for i in 0..50_000u32 {
            body.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&target, &body).unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        // limit large enough that the slice still exceeds the budget.
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "limit": 50_000 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds maximum allowed tokens (25000)"),
            "got: {msg}"
        );
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_read_failed"));
        assert!(!names.contains(&"tengu_tool_read_completed"));
    }

    #[tokio::test]
    async fn ranged_read_under_token_budget_of_huge_file_passes() {
        // The gate runs on the range-limited slice, not the whole file. A huge
        // file read with offset+limit returning a small slice must pass.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("huge.txt");
        // > token budget if read whole, but we slice 2 lines.
        let mut body = String::from("first\nsecond\nthird\n");
        while body.len() < 120_000 {
            body.push_str("filler-line\n");
        }
        std::fs::write(&target, &body).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 1, "limit": 2 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("small slice of a huge file passes the token gate");
        assert_eq!(result.data["file"]["content"], "first\nsecond\n");
    }

    #[tokio::test]
    async fn read_under_token_budget_passes() {
        // A small file is well under the budget — no token error.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("small.txt");
        std::fs::write(&target, "hello\nworld\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "hello\nworld\n");
    }

    #[test]
    fn model_facing_constants_byte_locked() {
        assert_eq!(
            EMPTY_FILE_WARNING,
            "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>"
        );
        assert_eq!(
            format_offset_beyond_eof(500, 12),
            "<system-reminder>Warning: the file exists but is shorter than the provided offset (500). The file has 12 lines.</system-reminder>"
        );
        // (removed) cyber-risk reminder assertions — that reminder does not exist
        // in claude-code v2.1.183 (parity verdict 12/14).
        assert!(FILE_UNCHANGED_STUB.starts_with("File unchanged since last read."));
    }

    // ───────────── FileReadTool tengu analytics events (3) ───────────────────

    /// Find the (single) recorded event with the given name.
    fn find_event<'a>(
        events: &'a [telemetry::sinks::in_memory::RecordedEvent],
        name: &str,
    ) -> Option<&'a telemetry::sinks::in_memory::RecordedEvent> {
        events.iter().find(|e| e.name == name)
    }

    #[test]
    fn analytics_ext_helper_matches_ts() {
        use std::path::Path;
        // Lowercased, no leading dot.
        assert_eq!(
            get_file_extension_for_analytics(Path::new("/a/b.RS")).as_deref(),
            Some("rs")
        );
        // No extension → None (TS `undefined`).
        assert_eq!(
            get_file_extension_for_analytics(Path::new("/a/README")),
            None
        );
        // Over MAX_FILE_EXTENSION_LENGTH (10) → "other".
        assert_eq!(
            get_file_extension_for_analytics(Path::new("/a/b.abcdefghijk")).as_deref(),
            Some("other")
        );
    }

    #[tokio::test]
    async fn session_file_read_fires_on_normal_text_read_with_counts() {
        // A normal (non-dedup, non-session) text read fires tengu_session_file_read
        // with the right line/byte counts and the only-if-present `ext`, and the
        // omitted `messageID` / false session flags.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("note.rs");
        std::fs::write(&target, "alpha\nbeta\ngamma\n").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["file"]["content"], "alpha\nbeta\ngamma\n");

        let events = sink.events().await;
        let ev = find_event(&events, "tengu_session_file_read")
            .expect("tengu_session_file_read must fire on a successful text read");
        let md = &ev.metadata;
        // totalLines = 4 (trailing-newline phantom final line, TS readFileInRange).
        assert!(matches!(md.get("totalLines"), Some(AnalyticsValue::Int(4))));
        // readLines = number of selected lines = total lines of the full read = 3
        // (split_inclusive over 3 newlines → 3 fragments).
        assert!(matches!(md.get("readLines"), Some(AnalyticsValue::Int(3))));
        // totalBytes = full content byte length; readBytes = selected slice bytes;
        // a full read makes them equal (17 bytes).
        assert!(matches!(
            md.get("totalBytes"),
            Some(AnalyticsValue::Int(17))
        ));
        assert!(matches!(md.get("readBytes"), Some(AnalyticsValue::Int(17))));
        // offset defaulted to 1; limit omitted (only-if-present).
        assert!(matches!(md.get("offset"), Some(AnalyticsValue::Int(1))));
        assert!(md.get("limit").is_none());
        // ext present (only-if-present spread): lowercased "rs".
        assert!(matches!(md.get("ext"), Some(AnalyticsValue::String(s)) if s == "rs"));
        // messageID omitted faithfully (unreachable in the port).
        assert!(md.get("messageID").is_none());
        // Not a session file → both flags false.
        assert!(matches!(
            md.get("is_session_memory"),
            Some(AnalyticsValue::Bool(false))
        ));
        assert!(matches!(
            md.get("is_session_transcript"),
            Some(AnalyticsValue::Bool(false))
        ));
    }

    #[tokio::test]
    async fn session_file_read_includes_limit_when_ranged() {
        // A ranged read carries `limit` (only-if-present) and the ranged counts.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("ranged.txt");
        std::fs::write(&target, "l1\nl2\nl3\nl4\n").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        tool.call(
            json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 2 }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let events = sink.events().await;
        let md = &find_event(&events, "tengu_session_file_read")
            .expect("session_file_read fires on ranged read")
            .metadata;
        assert!(matches!(md.get("offset"), Some(AnalyticsValue::Int(2))));
        assert!(matches!(md.get("limit"), Some(AnalyticsValue::Int(2))));
        // 2 selected lines ("l2\nl3\n").
        assert!(matches!(md.get("readLines"), Some(AnalyticsValue::Int(2))));
        // readBytes = bytes of "l2\nl3\n" = 6; totalBytes = full file = 12.
        assert!(matches!(md.get("readBytes"), Some(AnalyticsValue::Int(6))));
        assert!(matches!(
            md.get("totalBytes"),
            Some(AnalyticsValue::Int(12))
        ));
        // ext "txt" present.
        assert!(matches!(md.get("ext"), Some(AnalyticsValue::String(s)) if s == "txt"));
    }

    // ─────────────────── SEEDED Read dedup (source:"seeded") ────────────────
    //
    // Oracle @235741459:
    //   if(_&&_.seededFromContext&&!_.isPartialView&&t===1&&r===void 0)
    //     if(await b0e(f)===_.timestamp){
    //       let b=vq(f);
    //       return M("tengu_file_read_dedup",{source:Te("seeded"),...b!==void 0&&{ext:b}}),
    //              {data:{type:"file_unchanged",file:{filePath:f},source:"seeded"}}}
    //
    // Note `t===1` is the DEFAULTED offset (`call({file_path:e,offset:t=1,…})`
    // @235740900), so an explicit `offset:1` still dedups — unlike the
    // non-seeded branch, whose `rangeMatch` compares the raw `offset`.

    /// Seed `path` into the tool's read-state exactly as
    /// `ConversationOrchestrator::seed_memory_read_state` does.
    fn seed_entry(
        ctx: &BuiltinToolContext,
        path: &std::path::Path,
        content: &str,
        is_partial_view: bool,
    ) {
        let mtime_ms = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map(tool_api::read_file_state::mtime_ms_floor)
            .unwrap();
        tool_api::read_file_state::set_with_model_context(
            &ctx.read_file_state,
            path.to_path_buf(),
            tool_api::read_file_state::ReadFileEntry {
                content: content.to_string(),
                mtime_ms,
                offset: None,
                limit: None,
                from_read: false,
                seeded_from_context: true,
                is_partial_view,
            },
            true,
        );
    }

    #[tokio::test]
    async fn seeded_full_read_returns_the_seeded_stub() {
        // T1: seeded + not partial + no offset/limit + unchanged mtime.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("LINGXI.md");
        std::fs::write(&target, "# rules\nbe good\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let canon = std::fs::canonicalize(&target).unwrap();
        seed_entry(&ctx, &canon, "# rules\nbe good\n", false);
        let tool = FileReadTool::new(ctx);

        let r = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(r.data["type"], "file_unchanged");
        assert_eq!(r.data["source"], "seeded");
        // The stub names the LEXICAL path (`target`), not `canon`. The oracle's
        // `f` is `Li(e)` (@226620214) — `path.normalize`/`resolve`, no symlink
        // resolution — and the stub has to name the same path the memory block
        // above it printed. This expectation previously used `canon`, which only
        // agreed because the fixture happened to sit under a resolved path.
        assert_eq!(
            r.model_content.as_deref(),
            Some(
                format!(
                    "<system-reminder>This file is already in your context (see \"Contents of {}\" above) and has not changed on disk. Use that content instead of re-reading.</system-reminder>",
                    target.display()
                )
                .as_str()
            )
        );
    }

    #[tokio::test]
    async fn seeded_dedup_fires_on_explicit_offset_one() {
        // T2: the DEFAULTED-offset gate. `offset: 1` is what the oracle's
        // `t=1` default produces, so an explicit `offset:1` must still dedup —
        // even though the non-seeded branch's `rangeMatch` (which compares the
        // raw, un-defaulted `input_offset`) would refuse it.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("LINGXI.md");
        std::fs::write(&target, "# rules\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let canon = std::fs::canonicalize(&target).unwrap();
        seed_entry(&ctx, &canon, "# rules\n", false);
        let tool = FileReadTool::new(ctx);

        let r = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "offset": 1 }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(r.data["type"], "file_unchanged");
        assert_eq!(r.data["source"], "seeded");
    }

    #[tokio::test]
    async fn seeded_dedup_refuses_ranged_reads() {
        // T3: `t===1 && r===void 0` — any other offset, or ANY limit, skips
        // the seeded branch and returns the full/ranged content.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("LINGXI.md");
        std::fs::write(&target, "a\nb\nc\n").unwrap();
        let canon = std::fs::canonicalize(&target).unwrap();

        for input in [
            json!({ "file_path": target.to_str().unwrap(), "offset": 2 }),
            json!({ "file_path": target.to_str().unwrap(), "limit": 2 }),
        ] {
            let (ctx, _sink) = make_ctx(&tmp);
            seed_entry(&ctx, &canon, "a\nb\nc\n", false);
            let tool = FileReadTool::new(ctx);
            let r = tool
                .call(input.clone(), fresh_ctx(), fresh_tx())
                .await
                .unwrap();
            assert_ne!(
                r.data["type"], "file_unchanged",
                "ranged read must not dedup: {input}"
            );
            assert!(r.data["file"]["content"].is_string(), "{input}");
        }
    }

    #[tokio::test]
    async fn seeded_dedup_refuses_a_partial_view() {
        // T4: `!_.isPartialView` — a memory file whose recorded content differs
        // from disk (frontmatter/HTML comments stripped) must NOT dedup; the
        // model never saw the on-disk bytes.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cond.md");
        std::fs::write(&target, "---\npaths: src/**\n---\nbody\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let canon = std::fs::canonicalize(&target).unwrap();
        seed_entry(&ctx, &canon, "---\npaths: src/**\n---\nbody\n", true);
        let tool = FileReadTool::new(ctx);

        let r = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_ne!(r.data["type"], "file_unchanged");
        assert!(r.data["file"]["content"].is_string());
    }

    #[tokio::test]
    async fn seeded_dedup_refuses_when_mtime_changed() {
        // T5: `await b0e(f)===_.timestamp`.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("LINGXI.md");
        std::fs::write(&target, "v1\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let canon = std::fs::canonicalize(&target).unwrap();
        seed_entry(&ctx, &canon, "v1\n", false);
        // Rewrite with a clearly-advanced mtime.
        std::fs::write(&target, "v2\n").unwrap();
        filetime::set_file_mtime(
            &target,
            filetime::FileTime::from_unix_time(2_000_000_000, 0),
        )
        .unwrap();
        let tool = FileReadTool::new(ctx);

        let r = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_ne!(r.data["type"], "file_unchanged");
        assert_eq!(r.data["file"]["content"], "v2\n");
    }

    #[tokio::test]
    async fn seeded_dedup_telemetry_carries_source_and_ext_and_prior_op() {
        // T6: `{source:Te("seeded"), ...b!==void 0&&{ext:b}}` — BOTH keys; and
        // the sibling `tengu_file_read_reread` picks `priorOp:"seeded"`
        // (@235740900: `m.seededFromContext?"seeded":…`).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("LINGXI.md");
        std::fs::write(&target, "# rules\n").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let canon = std::fs::canonicalize(&target).unwrap();
        seed_entry(&ctx, &canon, "# rules\n", false);
        let tool = FileReadTool::new(ctx);

        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();

        let events = sink.events().await;
        let md = &find_event(&events, "tengu_file_read_dedup")
            .expect("seeded dedup must fire tengu_file_read_dedup")
            .metadata;
        assert!(matches!(md.get("source"), Some(AnalyticsValue::String(s)) if s == "seeded"));
        assert!(matches!(md.get("ext"), Some(AnalyticsValue::String(s)) if s == "md"));
        let rr = &find_event(&events, "tengu_file_read_reread")
            .expect("reread fires whenever an entry already exists")
            .metadata;
        assert!(matches!(rr.get("priorOp"), Some(AnalyticsValue::String(s)) if s == "seeded"));
    }

    #[tokio::test]
    async fn non_seeded_dedup_still_omits_source() {
        // T7 (regression): the ordinary Read→Read dedup keeps the SHORT form
        // and emits NO `source` key (the oracle's non-seeded return is
        // `{data:{type:"file_unchanged",file:{filePath:e}}}` — no third field).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("plain.rs");
        std::fs::write(&target, "alpha\n").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        sink.clear().await;
        let second = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(second.data["type"], "file_unchanged");
        assert!(second.data.get("source").is_none());
        assert_eq!(second.model_content.as_deref(), Some(FILE_UNCHANGED_SHORT));
        let events = sink.events().await;
        let md = &find_event(&events, "tengu_file_read_dedup")
            .expect("non-seeded dedup still fires")
            .metadata;
        assert!(md.get("source").is_none(), "non-seeded dedup has no source");
        let rr = &find_event(&events, "tengu_file_read_reread")
            .unwrap()
            .metadata;
        assert!(matches!(rr.get("priorOp"), Some(AnalyticsValue::String(s)) if s == "read"));
    }

    #[tokio::test]
    async fn dedup_path_fires_file_read_dedup_with_ext() {
        // The dedup short-circuit (file_unchanged) fires tengu_file_read_dedup
        // (and NOT a second tengu_session_file_read on the deduped read).
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dedup.rs");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        // First read populates the registry + fires session_file_read.
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        sink.clear().await;
        // Second identical read → dedup stub.
        let second = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(second.data["type"], "file_unchanged");
        let events = sink.events().await;
        let md = &find_event(&events, "tengu_file_read_dedup")
            .expect("dedup short-circuit must fire tengu_file_read_dedup")
            .metadata;
        // ext present (only-if-present): "rs".
        assert!(matches!(md.get("ext"), Some(AnalyticsValue::String(s)) if s == "rs"));
        // The deduped read does NOT fire session_file_read (it returns the stub
        // before the text-read site).
        assert!(find_event(&events, "tengu_session_file_read").is_none());
    }

    #[tokio::test]
    async fn limits_override_is_wired_but_never_fires() {
        // The LingXi ToolUseContext has no fileReadingLimits, so the override
        // event is wired (name registered) but never emitted — a faithful port
        // of a never-true branch.
        assert!(
            telemetry::tengu::ALL_EVENT_NAMES.contains(&"tengu_file_read_limits_override"),
            "the limits-override event NAME must be registered even though it never fires"
        );
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("plain.txt");
        std::fs::write(&target, "x\n").unwrap();
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        tool.call(
            json!({ "file_path": target.to_str().unwrap() }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let events = sink.events().await;
        // Unreachable branch (fileReadingLimits == None) → never emitted.
        assert!(find_event(&events, "tengu_file_read_limits_override").is_none());
    }

    #[test]
    fn detect_session_file_type_classifies_under_config_home() {
        // session_memory: <configHome>/.../session-memory/*.md
        // session_transcript: <configHome>/.../projects/*.jsonl
        // Build paths under the resolved config home (no process-global env
        // mutation — that would race other parallel tests). `lingxi_config_home_dir`
        // resolves the same way `detect_session_file_type` reads it.
        let config_home = lingxi_config_home_dir();
        let mem = config_home.join("agents/session-memory/abc.md");
        let trans = config_home.join("projects/foo/bar.jsonl");
        let plain = config_home.join("settings.json");
        assert_eq!(detect_session_file_type(&mem), Some("session_memory"));
        assert_eq!(detect_session_file_type(&trans), Some("session_transcript"));
        // .md under the config home but NOT under session-memory/ → None.
        assert_eq!(detect_session_file_type(&plain), None);
        // Outside the config home → None.
        assert_eq!(
            detect_session_file_type(std::path::Path::new("/definitely/not/claude/x.md")),
            None
        );
    }

    #[cfg(feature = "image-read")]
    #[tokio::test]
    async fn reads_image_into_result_data_with_no_injected_image_message() {
        // Build a tiny PNG via the `image` crate (available under
        // `--features image-read`) and write it under the trusted dir, mirroring
        // the sibling text-read `call()` tests' tool/ctx/temp-file construction.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("pic.png");
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&target, &buf).unwrap();

        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let res = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("image read must succeed");

        // Binary result shape: `{type:"image", file:{base64, type, originalSize}}`.
        // The dispatch loop derives the tool_result `content_blocks` from THIS
        // data (result-mapper `case "image"`), so the base64 + media type must be
        // present here — and the image must NOT also ride an injected message.
        assert_eq!(res.data["type"], "image");
        assert_eq!(res.data["file"]["type"], "image/png");
        assert!(res.data["file"]["base64"]
            .as_str()
            .is_some_and(|b| !b.is_empty()));
        // An 8x8 PNG is not resized → no dimensions, no resize-note message.
        assert!(res.data["file"].get("dimensions").is_none());
        assert!(
            res.new_messages.is_empty(),
            "unresized image must inject NO messages (the image rides the tool_result)"
        );
    }

    #[cfg(feature = "image-read")]
    #[tokio::test]
    async fn image_extension_with_html_content_gets_the_magic_byte_refusal() {
        // FT-09: `q_l`'s `Cle(o)===null` guard — a "download" that actually saved
        // a login page. Before FT-09 this fell through to the `image` crate's
        // `failed to decode image: …`.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("shot.png");
        std::fs::write(&target, b"<!DOCTYPE html><html><head><title>Login</title>").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let canon = std::fs::canonicalize(&target).unwrap();
        match err {
            ToolError::Io(m) => assert_eq!(
                m,
                crate::image_read::format_image_bad_magic(
                    &canon,
                    b"<!DOCTYPE html><html><head><title>Login</title>"
                )
            ),
            other => panic!("expected Io with the bad-magic message, got {other:?}"),
        }
    }

    #[cfg(feature = "image-read")]
    #[tokio::test]
    async fn empty_image_file_gets_the_empty_refusal() {
        // FT-09: `if(i===0)throw new ht(\`Image file is empty: ${e}\`, …)`.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("empty.png");
        std::fs::write(&target, b"").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let canon = std::fs::canonicalize(&target).unwrap();
        match err {
            ToolError::Io(m) => {
                assert_eq!(m, format!("Image file is empty: {}", canon.display()));
            }
            other => panic!("expected Io with the empty-image message, got {other:?}"),
        }
    }

    #[cfg(feature = "image-read")]
    #[tokio::test]
    async fn resized_image_injects_only_the_meta_resize_note() {
        use lingxi_core::types::{ContentBlock, ConversationMessage};

        // 4000px wide → exceeds IMAGE_MAX_DIM (2000) → resized.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("wide.png");
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(4000, 100))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&target, &buf).unwrap();

        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let res = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("image read must succeed");

        // Resized → data.file.dimensions present (claude tbo's dimensions).
        assert_eq!(res.data["file"]["dimensions"]["originalWidth"], 4000);
        // The ONLY injected message is the isMeta resize note — text, NO image
        // block (claude `newMessages: [Lr({content: y8t(dims), isMeta: true})]`).
        assert_eq!(res.new_messages.len(), 1);
        match &res.new_messages[0] {
            ConversationMessage::User {
                content, is_meta, ..
            } => {
                assert!(*is_meta, "resize note must be isMeta");
                assert!(
                    !content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::Image { .. })),
                    "note message must carry NO image block"
                );
                let text = content
                    .iter()
                    .find_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .expect("note text");
                assert!(text.starts_with("[Image: original 4000x100, displayed at "));
                assert!(text.ends_with(" to map to original image.]"));
            }
            other => panic!("expected a User message, got {other:?}"),
        }
    }

    /// A valid minimal 1-page PDF with a proper xref table + startxref. lopdf
    /// 0.34 rejects PDFs without an xref table, so this fixture carries one.
    #[cfg(feature = "pdf-read")]
    const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\nxref\n0 4\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n186\n%%EOF\n";

    #[cfg(feature = "pdf-read")]
    #[tokio::test]
    async fn pdf_pages_over_cap_errors() {
        // A `pages` range spanning >20 pages is refused with the byte-locked
        // "exceeds maximum" error, before any extraction is attempted. This
        // exercises the routing regardless of whether the bytes parse.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("doc.pdf");
        std::fs::write(&target, MINIMAL_PDF).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap(), "pages": "1-25" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "got: {err}");
    }

    #[cfg(feature = "pdf-read")]
    #[tokio::test]
    async fn non_pdf_with_pdf_extension_is_rejected_before_inlining() {
        // A non-PDF renamed `.pdf` must NOT be inlined as a document block — an
        // invalid PDF block poisons the conversation (every later API call 400s).
        // The %PDF- guard rejects it instead.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("fake.pdf");
        std::fs::write(&target, b"this is not a pdf at all").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a valid PDF"), "got: {err}");
    }

    #[cfg(feature = "pdf-read")]
    #[tokio::test]
    async fn reads_small_pdf_as_inline_document() {
        use lingxi_core::types::{ContentBlock, ConversationMessage, DocumentSource};

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("doc.pdf");
        std::fs::write(&target, MINIMAL_PDF).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let res = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("small PDF read must succeed");

        // The tool-result data is text-only and tagged as a PDF.
        assert_eq!(res.data["type"], "pdf");

        // The PDF bytes ride on a single `new_messages` user message carrying a
        // base64 `ContentBlock::Document` with the application/pdf media type.
        assert_eq!(res.new_messages.len(), 1);
        let content = match &res.new_messages[0] {
            ConversationMessage::User { content, .. } => content,
            other => panic!("expected a User message, got {other:?}"),
        };
        let media_type = content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Document {
                    source: DocumentSource::Base64 { media_type, .. },
                } => Some(media_type.as_str()),
                _ => None,
            })
            .expect("a base64 document block");
        assert_eq!(media_type, "application/pdf");
    }

    #[cfg(feature = "pdf-read")]
    #[tokio::test]
    async fn unsupported_model_pdf_returns_not_supported_message() {
        // No-pages read on an UNSUPPORTED model (claude-3-haiku) is refused with
        // claude-code's exact message (FileReadTool.ts:979-985) — never inlined.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("doc.pdf");
        std::fs::write(&target, MINIMAL_PDF).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let mut call_ctx = fresh_ctx();
        call_ctx.options.main_loop_model = "claude-3-haiku-20240307".into();
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                call_ctx,
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(
                "Reading full PDFs is not supported with this model. Use a newer model (Sonnet 3.5 v2 or later), or use the pages parameter to read specific page ranges (e.g., pages: \"1-5\", maximum 20 pages per request). Page extraction requires poppler-utils: install with `brew install poppler` on macOS or `apt-get install poppler-utils` on Debian/Ubuntu."
            ),
            "got: {err}"
        );
    }

    #[cfg(feature = "pdf-read")]
    #[tokio::test]
    async fn supported_model_inlines_pdf_between_3mb_and_20mb() {
        // claude-code inlines a supported-model PDF up to PDF_TARGET_RAW_SIZE
        // (20 MB); the old 3 MB error was a P4a simplification. A ~4 MB PDF must
        // inline as a Document block, NOT error. (Padding breaks lopdf parsing →
        // pdf_page_count None → the >10-page gate is skipped, which is fine here.)
        use lingxi_core::types::{ContentBlock, ConversationMessage, DocumentSource};
        let mut bytes = vec![b'%'; 4 * 1024 * 1024];
        bytes[..9].copy_from_slice(b"%PDF-1.4\n");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("big.pdf");
        std::fs::write(&target, &bytes).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let res = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("4MB supported-model PDF must inline, not error");
        assert_eq!(res.data["type"], "pdf");
        assert_eq!(res.new_messages.len(), 1);
        let content = match &res.new_messages[0] {
            ConversationMessage::User { content, .. } => content,
            other => panic!("expected a User message, got {other:?}"),
        };
        assert!(
            content.iter().any(|b| matches!(
                b,
                ContentBlock::Document {
                    source: DocumentSource::Base64 { .. }
                }
            )),
            "expected an inline base64 document block"
        );
    }

    #[cfg(feature = "pdf-read")]
    #[tokio::test]
    async fn supported_model_oversize_pdf_returns_too_large() {
        // > 20 MB (PDF_TARGET_RAW_SIZE) ⇒ too_large with the human-readable size
        // (claude-code readPDF, utils/pdf.ts:60-68).
        let mut bytes = vec![b'%'; 20 * 1024 * 1024 + 1];
        bytes[..9].copy_from_slice(b"%PDF-1.4\n");
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("huge.pdf");
        std::fs::write(&target, &bytes).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": target.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("PDF file exceeds maximum allowed size of 20MB."),
            "got: {err}"
        );
    }

    #[cfg(feature = "pdf-render")]
    #[test]
    fn build_pages_payload_emits_one_image_per_page() {
        use super::build_pages_payload;
        use image::{DynamicImage, RgbImage};

        fn jpeg(w: u32, h: u32) -> Vec<u8> {
            let img = DynamicImage::ImageRgb8(RgbImage::new(w, h));
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
            buf.into_inner()
        }

        let pages = vec![jpeg(50, 50), jpeg(40, 60)];
        let (sources, data, model_content) =
            build_pages_payload(std::path::Path::new("/docs/report.pdf"), 4096, pages).unwrap();

        assert_eq!(sources.len(), 2, "one ImageSource per rendered page");
        for s in &sources {
            match s {
                lingxi_core::types::ImageSource::Base64 { media_type, data } => {
                    assert_eq!(media_type, "image/jpeg");
                    assert!(!data.is_empty());
                }
                other => panic!("expected Base64 source, got {other:?}"),
            }
        }
        assert_eq!(data["type"], "parts");
        assert_eq!(data["file"]["filePath"], "/docs/report.pdf");
        assert_eq!(data["file"]["count"], 2);
        assert!(
            data.get("model_content").is_none(),
            "render rides on the channel"
        );
        assert_eq!(
            model_content,
            "PDF pages extracted: 2 page(s) from /docs/report.pdf (4KB)"
        );
    }

    #[cfg(feature = "pdf-read")]
    #[test]
    fn format_file_size_matches_claude_code() {
        use super::format_file_size;
        assert_eq!(format_file_size(0), "0 bytes");
        assert_eq!(format_file_size(512), "512 bytes");
        assert_eq!(format_file_size(1024), "1KB"); // 1.0 → trim .0
        assert_eq!(format_file_size(1536), "1.5KB");
        assert_eq!(format_file_size(3 * 1024 * 1024), "3MB");
        assert_eq!(format_file_size(20 * 1024 * 1024), "20MB");
        assert_eq!(format_file_size(100 * 1024 * 1024), "100MB");
        assert_eq!(format_file_size(2 * 1024 * 1024 * 1024), "2GB");
    }

    // ── Missing-file UX (`FileReadTool.ts:608-649`) ──────────────────────────

    #[test]
    fn find_similar_file_matches_same_stem_sibling() {
        // `findSimilarFile` (`utils/file.ts:178-207`): same base name, different
        // extension, in the same directory. `App.tsx` is missing; `App.ts` exists.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("App.ts"), "x").unwrap();
        let missing = tmp.path().join("App.tsx");
        assert_eq!(
            find_similar_file(&missing).as_deref(),
            Some("App.ts"),
            "should suggest the same-stem sibling"
        );
    }

    #[test]
    fn find_similar_file_none_when_no_sibling_shares_stem() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Other.ts"), "x").unwrap();
        let missing = tmp.path().join("App.tsx");
        assert_eq!(find_similar_file(&missing), None);
    }

    #[test]
    fn get_alternate_screenshot_path_swaps_space_for_thin_space() {
        // `getAlternateScreenshotPath` (`FileReadTool.ts:147-159`): a regular
        // space before AM/PM swaps to U+202F (and vice-versa).
        let regular = std::path::Path::new("/tmp/Screenshot 2024-01-01 at 3.04.05 PM.png");
        let alt = get_alternate_screenshot_path(regular).unwrap();
        assert_eq!(
            alt.to_str().unwrap(),
            "/tmp/Screenshot 2024-01-01 at 3.04.05\u{202F}PM.png"
        );
        // Round-trips back to a regular space.
        let back = get_alternate_screenshot_path(&alt).unwrap();
        assert_eq!(back, regular);
        // AM works too.
        let am = std::path::Path::new("/tmp/Screenshot 9.00.00 AM.png");
        assert_eq!(
            get_alternate_screenshot_path(am).unwrap().to_str().unwrap(),
            "/tmp/Screenshot 9.00.00\u{202F}AM.png"
        );
        // Non-screenshot names (no AM/PM, no .png, or nothing before the space)
        // return None.
        assert!(get_alternate_screenshot_path(std::path::Path::new("/tmp/notes.txt")).is_none());
        assert!(get_alternate_screenshot_path(std::path::Path::new("/tmp/file PM.txt")).is_none());
        assert!(get_alternate_screenshot_path(std::path::Path::new("/tmp/ PM.png")).is_none());
    }

    #[tokio::test]
    async fn missing_file_with_near_match_suggests_did_you_mean() {
        // (c) A missing path with a same-stem sibling but no cwd-correction
        // (the file is directly under cwd, so `suggest_path_under_cwd` declines)
        // falls back to the sibling: the base cwd-note message + the byte-locked
        // `" Did you mean {name}?"` sibling suffix (`FileReadTool.ts:644-645`).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.json"), "{}").unwrap();
        let missing = tmp.path().join("config.yaml");
        let (ctx, sink) = make_ctx(&tmp);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": missing.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        // Full message: base cwd-note + the sibling suggestion suffix.
        let cwd = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(
            msg.contains(&format!(
                "File does not exist. {FILE_NOT_FOUND_CWD_NOTE} {}.",
                cwd.display()
            )),
            "expected the base cwd-note message, got: {msg}"
        );
        assert!(
            msg.ends_with(" Did you mean config.json?"),
            "expected the verbatim TS sibling-suggestion suffix, got: {msg}"
        );
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_read_failed"));
    }

    #[tokio::test]
    async fn missing_file_under_cwd_parent_suggests_corrected_path_with_precedence() {
        // (a) The "dropped repo folder" pattern (`suggestPathUnderCwd`,
        // `utils/file.ts:228-267`): cwd = base/repo, a missing path under base
        // (cwd's PARENT) but NOT under cwd, whose SAME relative path exists under
        // cwd, yields the corrected-path `" Did you mean {correctedPath}?"`
        // suffix — and that cwd suggestion WINS over a same-stem sibling that
        // also exists in the requested directory (`FileReadTool.ts:642-645`).
        //
        // The requested path must validate under the trusted dirs (the read
        // path canonicalize-validates BEFORE the not-found arm), so `base` is a
        // trusted dir while the cwd/workspace is `base/repo`.
        let base = TempDir::new().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        // The corrected path that SHOULD be suggested: base/repo/sub/app.config.
        let corrected_dir = repo.join("sub");
        std::fs::create_dir(&corrected_dir).unwrap();
        let corrected = corrected_dir.join("app.config");
        std::fs::write(&corrected, "ok").unwrap();

        // The requested (missing) path, under base (cwd's parent) but not cwd:
        // base/sub/app.config. Its parent dir must exist for canonicalize to
        // resolve it; add a same-stem sibling there too (app.json) to prove the
        // cwd suggestion takes PRECEDENCE over `find_similar_file`.
        let requested_dir = base.path().join("sub");
        std::fs::create_dir(&requested_dir).unwrap();
        std::fs::write(requested_dir.join("app.json"), "{}").unwrap();
        let missing = requested_dir.join("app.config");

        // Build the ctx with BOTH base and base/repo trusted (so the request
        // under base validates), then point the workspace (the `getCwd` analog)
        // at base/repo.
        let fs = make_dummy_fs();
        let bus = Arc::new(AnalyticsBus::new());
        let ctx = tool_api::test_support::ctx_for_file_tools(
            fs,
            bus,
            vec![base.path().to_path_buf(), repo.clone()],
        );
        // Point the cwd (the `getCwd` analog) at base/repo, keeping BOTH
        // base and base/repo trusted (so the request under base validates).
        ctx.session_cwd.swap(repo.clone(), ctx.trusted_dirs());
        let tool = FileReadTool::new(ctx);

        let err = tool
            .call(
                json!({ "file_path": missing.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();

        // Base cwd-note message: cwd = canonicalized base/repo.
        let cwd = std::fs::canonicalize(&repo).unwrap();
        assert!(
            msg.contains(&format!(
                "File does not exist. {FILE_NOT_FOUND_CWD_NOTE} {}.",
                cwd.display()
            )),
            "expected the base cwd-note message, got: {msg}"
        );
        // The cwd suggestion (corrected path) WINS — not the same-stem sibling.
        let corrected_canon = std::fs::canonicalize(&corrected).unwrap();
        assert!(
            msg.ends_with(&format!(" Did you mean {}?", corrected_canon.display())),
            "expected the cwd-corrected-path suggestion to take precedence, got: {msg}"
        );
        // And specifically NOT the sibling.
        assert!(
            !msg.contains("Did you mean app.json?"),
            "the cwd suggestion must win over the same-stem sibling, got: {msg}"
        );
    }

    #[tokio::test]
    async fn missing_file_without_near_match_returns_base_cwd_note() {
        // (b) No same-stem sibling AND no cwd-correction ⇒ just the base
        // `File does not exist. ${FILE_NOT_FOUND_CWD_NOTE} ${cwd}.` message
        // (`FileReadTool.ts:641`), with NO " Did you mean" suffix appended.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("unrelated.txt"), "x").unwrap();
        let missing = tmp.path().join("nope.md");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let err = tool
            .call(
                json!({ "file_path": missing.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        // Base cwd-note message — the cwd is the (canonicalized) workspace = tmp.
        let cwd = std::fs::canonicalize(tmp.path()).unwrap();
        assert!(
            msg.contains(&format!(
                "File does not exist. {FILE_NOT_FOUND_CWD_NOTE} {}.",
                cwd.display()
            )),
            "expected the base cwd-note message, got: {msg}"
        );
        assert!(
            !msg.contains("Did you mean"),
            "no sibling + no cwd-correction ⇒ no suggestion, got: {msg}"
        );
        // Still an Io error variant.
        assert!(matches!(err, ToolError::Io(_)), "got: {err:?}");
    }

    // ── P2-08: the "File does not exist" note prints the LIVE cwd ─────────────

    #[tokio::test]
    async fn live_cwd_cell_drives_file_not_found_note() {
        // A missing file under the workspace validates, reaching the not-found
        // arm; its cwd-note must print the injected LIVE cwd (tmp/sub), NOT the
        // workspace (tmp) — 1:1 with claude-code's `getCwd()`/`Ct()`.
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let missing = tmp.path().join("missing.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let cell = std::sync::Arc::new(std::sync::Mutex::new(sub.clone()));
        let tool = FileReadTool::new(ctx).with_live_cwd(cell);
        let err = tool
            .call(
                json!({ "file_path": missing.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        // The note shows the canonicalized LIVE cwd (tmp/sub), ending in `/sub.`.
        let canon_sub = std::fs::canonicalize(&sub).unwrap();
        assert!(
            msg.contains(&format!(
                "File does not exist. {FILE_NOT_FOUND_CWD_NOTE} {}.",
                canon_sub.display()
            )),
            "expected the note to print the live cwd (tmp/sub), got: {msg}"
        );
    }

    #[tokio::test]
    async fn screenshot_regular_space_resolves_to_thin_space_file() {
        // macOS screenshot AM/PM fallback (`FileReadTool.ts:612-636`): the model
        // passes a regular space, the real file on disk uses U+202F. The read
        // succeeds by retrying the alternate-space path.
        let tmp = TempDir::new().unwrap();
        // Real file uses the thin space (U+202F) before PM.
        let real = tmp
            .path()
            .join("Screenshot 2024-01-01 at 3.04.05\u{202F}PM.png");
        std::fs::write(&real, "PNG-BYTES-NOT-REALLY").unwrap();
        // Requested path uses a regular space.
        let requested = tmp.path().join("Screenshot 2024-01-01 at 3.04.05 PM.png");
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                json!({ "file_path": requested.to_str().unwrap() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        // With `image-read` on, a `.png` routes to the image path and the
        // (fake) bytes fail to decode — but the point under test is that the
        // space-swap retry RESOLVED the file (we got past the not-found arm), so
        // the error must NOT be a file-not-found / "Did you mean". With
        // `image-read` off, a `.png` is a binary-extension reject — again past
        // the not-found arm. Either way: no not-found error.
        match result {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    !msg.contains("Did you mean") && !msg.contains("No such file"),
                    "screenshot space-swap should have resolved the file (got past \
                     the not-found arm), but error was: {msg}"
                );
            }
        }
    }
}
