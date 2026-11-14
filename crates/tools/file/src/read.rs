//! `FileReadTool` — Claude Code 2.1.322 file-reading contracts.
//!
//! The ordinary byte cap is 262,144; trusted context room can raise it for an
//! explicit `allow_large` request. Text uses UTF-8 or BOM-selected UTF-16LE,
//! LF-delimited ranges and UTF-16 token estimates. Unsupported text encodings
//! return a warning; mutation tools refuse them. NUL classification examines
//! the first 8,000 bytes. Legacy unchanged-file results remain readable, while
//! current calls always perform fresh I/O.

use crate::shared::NUL_SCAN_WINDOW;
use crate::Utf16JsonProjection;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    FILE_READ_LIMITS_OVERRIDE, FILE_READ_REREAD, READ_COMPLETED, READ_FAILED, READ_STARTED,
    SESSION_FILE_READ,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::util::ids::ulid_or_uuid;
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, translate_model_path,
};
use tool_api::BuiltinToolContext;

/// Ordinary Read byte limit; `allow_large` can raise it from trusted context room.
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
/// whose estimated token count exceeds this returns a partial first page;
/// explicit ranges fail with [`format_max_tokens_exceeded`].
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 25_000;

/// LingXi env tier for Claude-compatible per-call file-reading limits.
pub const MAX_OUTPUT_TOKENS_ENV: &str = "LINGXI_FILE_READ_MAX_OUTPUT_TOKENS";

const STREAMING_READ_CHUNK_BYTES: usize = 64 * 1024;

// Retain the permit inside each blocking job, including after its caller is
// dropped, so cancelled reads cannot bypass the bound while they finish a
// blocking syscall or codec operation.
const MAX_BLOCKING_READ_JOBS: usize = 4;
static BLOCKING_READ_JOBS: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(MAX_BLOCKING_READ_JOBS)));

fn check_worker_cancelled(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError::Aborted)
    } else {
        Ok(())
    }
}

/// Run owned file/codec work without occupying an async runtime worker. Each
/// job gets a child token cancelled when this future is dropped; synchronous
/// loops check it between chunks. An in-progress OS syscall or codec call
/// remains uninterruptible, and keeps its concurrency permit until it returns.
pub(crate) async fn run_read_job<T, F>(cancel: &CancellationToken, work: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce(&CancellationToken) -> Result<T, ToolError> + Send + 'static,
{
    check_worker_cancelled(cancel)?;
    let permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(ToolError::Aborted),
        permit = BLOCKING_READ_JOBS.clone().acquire_owned() =>
            permit.map_err(|error| ToolError::Io(error.to_string().into()))?,
    };
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        check_worker_cancelled(&child)?;
        let result = work(&child);
        check_worker_cancelled(&child)?;
        result
    });
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ToolError::Aborted),
        result = job => result.map_err(|error| ToolError::Io(error.to_string().into()))?,
    }
}

const ALLOW_LARGE_HINT: &str = " To read it anyway, up to what still fits in your context, call Read again with allow_large: true — only if you genuinely need all of it or the user asked for the whole file.";
const CONTEXT_ROOM_HINT: &str = " Even with allow_large, this is more than fits in the context you have left this turn (other large reads in the same turn share that room).";
const ALLOW_LARGE_UNAVAILABLE_HINT: &str =
    " allow_large is not available where this read runs; read it in parts with offset and limit.";

#[derive(Clone, Copy)]
struct ReadLimits {
    max_size_bytes: u64,
    max_tokens: u64,
}

#[derive(Clone, Copy)]
struct ReadRoom {
    room_tokens: Option<u64>,
    raised: Option<ReadLimits>,
}

struct ReadAllowance<'a> {
    room: Option<&'a tool_api::read_context_room::ReadContextRoom>,
    base: ReadLimits,
    model_bytes_per_token: u64,
    opted_in: bool,
}

/// Bound direct invocations as well as the dispatcher's native schema gate.
/// JavaScript's integer schema rejects values outside the safe-integer range;
/// a malformed optional value must never silently become an unbounded read.
fn read_range_argument(
    input: &Utf16JsonProjection,
    name: &str,
    positive: bool,
) -> Result<Option<u64>, ToolError> {
    let Some(value) = input.get_property(name) else {
        return Ok(None);
    };
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    let number = value.as_number().filter(|number| {
        number.is_finite()
            && number.fract() == 0.0
            && *number >= if positive { 1.0 } else { 0.0 }
            && *number <= MAX_SAFE_INTEGER
    });
    number.map(|number| Some(number as u64)).ok_or_else(|| {
        ToolError::InvalidInput(
            format!(
                "{name} must be a {} safe integer (at most 9007199254740991)",
                if positive { "positive" } else { "non-negative" }
            )
            .into(),
        )
    })
}

impl ReadAllowance<'_> {
    fn current_room(&self) -> ReadRoom {
        let room_tokens = self
            .room
            .filter(|room| room.has_context_room())
            .map(|room| room.room_tokens(0));
        ReadRoom {
            room_tokens,
            raised: room_tokens
                .filter(|tokens| *tokens > self.base.max_tokens)
                .map(|tokens| ReadLimits {
                    max_tokens: tokens,
                    max_size_bytes: self.base.max_size_bytes.max(
                        tokens
                            .saturating_mul(self.model_bytes_per_token)
                            .saturating_mul(3),
                    ),
                }),
        }
    }

    fn hint(&self, bytes: Option<u64>, tokens: Option<u64>) -> &'static str {
        let room = self.current_room();
        if room.room_tokens.is_none() {
            return if self.opted_in {
                ALLOW_LARGE_UNAVAILABLE_HINT
            } else {
                ""
            };
        }
        if self.opted_in || room.raised.is_none() {
            return CONTEXT_ROOM_HINT;
        }
        let raised = room.raised.expect("checked above");
        if bytes.is_none_or(|bytes| bytes <= raised.max_size_bytes)
            && tokens.is_none_or(|tokens| tokens <= raised.max_tokens)
        {
            ALLOW_LARGE_HINT
        } else {
            CONTEXT_ROOM_HINT
        }
    }
}

fn model_bytes_per_token(model: &str) -> u64 {
    if model.is_empty() {
        return 4;
    }
    let canonical =
        lingxi_core::host::model_capabilities::normalize_model_id(model).replace(['.', '_'], "-");
    if matches!(
        canonical.as_str(),
        "claude-3-opus"
            | "claude-3-sonnet"
            | "claude-3-haiku"
            | "claude-3-5-sonnet"
            | "claude-3-5-haiku"
            | "claude-3-7-sonnet"
            | "claude-opus-4-0"
            | "claude-opus-4-1"
            | "claude-opus-4-5"
            | "claude-opus-4-6"
            | "claude-sonnet-4-0"
            | "claude-sonnet-4-5"
            | "claude-sonnet-4-6"
            | "claude-haiku-4-5"
    ) {
        4
    } else {
        3
    }
}

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
/// nonnegative UTF-16 length is `(len + bpt/2) / bpt`. JavaScript string length
/// counts UTF-16 code units, including two units for supplementary characters.
#[must_use]
fn rough_token_count_estimation_for_file_type(content: &str, ext: Option<&str>) -> u64 {
    let bpt = bytes_per_token_for_file_type(ext);
    u64::try_from(content.encode_utf16().count())
        .unwrap_or(u64::MAX)
        .saturating_add(bpt / 2)
        / bpt
}

fn count_read_tokens(content: &str, ext: Option<&str>, offset: u64, raised: bool) -> u64 {
    if !raised {
        return rough_token_count_estimation_for_file_type(content, ext);
    }
    let numbered = add_line_numbers(content, offset.max(1));
    let estimated = rough_token_count_estimation_for_file_type(&numbered, ext);
    if estimated == 0 {
        return 0;
    }
    estimated.max((numbered.len() as u64).div_ceil(2))
}

async fn refined_read_tokens(
    allowance: &ReadAllowance<'_>,
    units: &[u16],
    ext: Option<&str>,
    offset: u64,
    raised: bool,
    max_tokens: u64,
    cancel: &CancellationToken,
) -> Result<u64, ToolError> {
    let display = String::from_utf16_lossy(units);
    let estimate = count_read_tokens(&display, ext, offset, raised);
    let Some(room) = allowance.room else {
        return Ok(estimate);
    };
    if estimate == 0 || estimate <= max_tokens / 4 {
        return Ok(estimate);
    }
    let source = crate::utf16_mutation::string(units);
    let counted = if raised {
        add_line_numbers_source(&source, offset.max(1))
    } else {
        source
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ToolError::Aborted),
        tokens = room.refine_tokens(&counted, estimate, max_tokens) => Ok(tokens),
    }
}

fn admit_read_tokens<'a>(
    allowance: &ReadAllowance<'a>,
    initial: ReadLimits,
    raised: bool,
    tokens: u64,
) -> Result<Option<tool_api::read_context_room::ReadTokenClaim<'a>>, u64> {
    if tokens > initial.max_tokens {
        return Err(initial.max_tokens);
    }
    if raised && tokens > 0 {
        if let Some(room) = allowance.room {
            return room
                .try_reserve(tokens, initial.max_tokens, allowance.base.max_tokens)
                .map(Some);
        }
    }
    Ok(None)
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
/// The pure helper below is the local fallback used in tests. Production uses
/// the host's route-pinned exact counter through `refined_read_tokens`; absent
/// or unsupported counters select this fallback: `apiCount` is `None`, hence
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
    units: Vec<u16>,
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
/// Lengths and slices use JavaScript's UTF-16 units, with the native high
/// surrogate guard preventing a prefix from splitting a surrogate pair.
fn truncate_to_token_budget(
    content: &str,
    token_count: u64,
    max_tokens: u64,
    total_lines: u64,
    full_file_path: &str,
) -> GracefulTruncation {
    truncate_units_to_token_budget(
        &content.encode_utf16().collect::<Vec<_>>(),
        token_count,
        max_tokens,
        total_lines,
        full_file_path,
    )
}

fn truncate_units_to_token_budget(
    content: &[u16],
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
    // `O(V) = V.length / N` — JavaScript's UTF-16 length.
    let est_tokens = |text: &[u16]| (text.len() as f64) / n_ratio;

    // Split into lines for the line-based shrink (`D = f.split("\n")`). This is a
    // plain (non-inclusive) split, matching claude-code's `f.split("\n")`.
    let lines: Vec<&[u16]> = content.split(|unit| *unit == 10).collect();
    let num_lines = lines.len();

    // `$ = max(1, min(D.length, floor(D.length * l / tokenCount * 0.85)))`.
    let mut line_n: usize = ((num_lines as f64) * cap / token_count_f * 0.85)
        .floor()
        .max(1.0) as usize;
    line_n = line_n.clamp(1, num_lines);
    let mut head = lines[..line_n].join(&10);

    // Up to 6 iterations of `$ *= 0.7` until it fits or `$ <= 1`.
    for _ in 0..6 {
        if est_tokens(&head) <= cap || line_n <= 1 {
            break;
        }
        line_n = ((line_n as f64) * 0.7).floor().max(1.0) as usize;
        head = lines[..line_n].join(&10);
    }

    let mut char_based = false;
    // Char-based fallback when the line shrink couldn't fit / produced blank.
    if est_tokens(&head) > cap
        || lingxi_core::host::effort::trim_js_whitespace(&String::from_utf16_lossy(&head))
            .is_empty()
    {
        // `V = max(1, floor(l * N * 0.85))` UTF-16 budget.
        let mut unit_budget: usize = (cap * n_ratio * 0.85).floor().max(1.0) as usize;
        for _ in 0..6 {
            head = content[..content.len().min(unit_budget)].to_vec();
            if est_tokens(&head) <= cap {
                break;
            }
            unit_budget = ((unit_budget as f64) * 0.7).floor().max(1.0) as usize;
        }
        if head
            .last()
            .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
        {
            head.pop();
        }
        char_based = true;
    }

    // `T = U, S = W ? Uu(U,"\n")+1 : $`.
    let line_count = if char_based {
        head.iter().filter(|unit| **unit == 10).count() as u64 + 1
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
        content: String::from_utf16_lossy(&head),
        units: head,
        line_count,
        note,
    }
}

/// Native `slice(0, unit_budget)` followed by its trailing-high-surrogate guard.
fn slice_utf16_on_char_boundary(content: &str, unit_budget: usize) -> String {
    let mut units = 0;
    let mut end = 0;
    for character in content.chars() {
        let next_units = units + character.len_utf16();
        if next_units > unit_budget {
            break;
        }
        units = next_units;
        end += character.len_utf8();
    }
    content[..end].to_string()
}

/// The native buffered range reader splits on LF, strips one CR from each
/// selected line and joins the selected lines with LF. Its terminal empty line
/// is a real range entry, so both empty files and trailing-LF files count it.
fn select_text_range(content: &str, offset: u64, limit: Option<u64>) -> StreamedTextRange {
    select_text_range_units(&content.encode_utf16().collect::<Vec<_>>(), offset, limit)
}

fn select_text_range_units(content: &[u16], offset: u64, limit: Option<u64>) -> StreamedTextRange {
    let content = content.strip_prefix(&[0xfeff]).unwrap_or(content);
    let offset = offset.max(1);
    let mut total_lines = 0;
    let mut selected = Vec::new();
    let end_line = limit.map(|limit| offset.saturating_add(limit));
    for (index, line) in content.split(|unit| *unit == 10).enumerate() {
        let line_number = index as u64 + 1;
        total_lines += 1;
        if line_number >= offset && end_line.is_none_or(|end| line_number < end) {
            selected.push(line.strip_suffix(&[13]).unwrap_or(line));
        }
    }
    let units = selected.join(&10);
    StreamedTextRange {
        content: String::from_utf16_lossy(&units),
        units,
        total_lines,
        read_lines: selected.len() as u64,
        not_text: None,
    }
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
    units: Vec<u16>,
    total_lines: u64,
    read_lines: u64,
    not_text: Option<crate::file_meta::NotText>,
}

/// Incremental Node-compatible UTF-8/UTF-16LE decoding. Keep incomplete scalar
/// bytes between chunks so a split code point never creates a spurious warning.
#[derive(Default)]
struct NativeTextDecoder {
    pending: Vec<u8>,
    started: bool,
    utf16le: bool,
    emitted: bool,
    invalid_utf8: bool,
}

fn check_selected_range_size(bytes: usize, limit: usize) -> Result<(), ToolError> {
    if bytes <= limit {
        return Ok(());
    }
    Err(ToolError::Io(format!(
        "The requested line range contains over {} of text, more than a read can return. Use a smaller limit — or, if a single line is this large, no limit will fit it: search for specific content instead.",
        format_file_size(limit as u64),
    ).into()))
}

impl NativeTextDecoder {
    fn decode(&mut self, chunk: &[u8], eof: bool) -> Vec<u16> {
        self.pending.extend_from_slice(chunk);
        if !self.started {
            if self.pending.len() < 2 && !eof {
                return Vec::new();
            }
            self.started = true;
            self.utf16le = self.pending.starts_with(&[0xff, 0xfe]);
        }
        let mut text = Vec::new();
        if self.utf16le {
            let consumed = self.pending.len() / 2 * 2;
            text.extend(
                self.pending[..consumed]
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
            );
            self.pending.drain(..consumed);
            if eof {
                self.pending.clear();
            }
        } else {
            let mut consumed = 0;
            while consumed < self.pending.len() {
                match std::str::from_utf8(&self.pending[consumed..]) {
                    Ok(valid) => {
                        text.extend(valid.encode_utf16());
                        consumed = self.pending.len();
                    }
                    Err(error) => {
                        let valid_end = consumed + error.valid_up_to();
                        text.extend(
                            std::str::from_utf8(&self.pending[consumed..valid_end])
                                .expect("validated prefix")
                                .encode_utf16(),
                        );
                        if let Some(length) = error.error_len() {
                            self.invalid_utf8 = true;
                            text.push(0xfffd);
                            consumed = valid_end + length;
                        } else if eof {
                            self.invalid_utf8 = true;
                            text.push(0xfffd);
                            consumed = self.pending.len();
                        } else {
                            consumed = valid_end;
                            break;
                        }
                    }
                }
            }
            self.pending.drain(..consumed);
        }
        if !self.emitted && !text.is_empty() {
            self.emitted = true;
            if text.first() == Some(&0xfeff) {
                text.remove(0);
            }
        }
        text
    }
}

// Node's UTF-8 byte length repairs unpaired surrogates, while a complete
// surrogate pair costs four bytes. A pending high surrogate was charged three
// bytes, so the following low surrogate adds only one, even across chunks.
fn appended_unit_utf8_bytes(previous: Option<u16>, unit: u16) -> usize {
    if (0xdc00..=0xdfff).contains(&unit)
        && previous.is_some_and(|prev| (0xd800..=0xdbff).contains(&prev))
    {
        1
    } else if (0xd800..=0xdfff).contains(&unit) {
        3
    } else {
        char::from_u32(u32::from(unit))
            .expect("non-surrogate BMP")
            .len_utf8()
    }
}

fn read_text_range_streaming<R: Read>(
    mut reader: R,
    _canon: &Path,
    offset: u64,
    limit: u64,
    _ext: Option<&str>,
    max_selected_bytes: u64,
    cancel: &CancellationToken,
) -> Result<StreamedTextRange, ToolError> {
    let offset = offset.max(1);
    let end_line = offset.saturating_add(limit);
    let max_selected_bytes = usize::try_from(max_selected_bytes).unwrap_or(usize::MAX);
    let mut selected = Vec::new();
    let mut current_line = 1_u64;
    let mut line = Vec::new();
    let mut selected_bytes = 0_usize;
    let mut prefix = Vec::with_capacity(NUL_SCAN_WINDOW);
    let mut decoder = NativeTextDecoder::default();
    let mut buf = vec![0u8; STREAMING_READ_CHUNK_BYTES];
    loop {
        check_worker_cancelled(cancel)?;
        let n = reader
            .read(&mut buf)
            .map_err(|error| ToolError::Io(error.to_string().into()))?;
        check_worker_cancelled(cancel)?;
        let chunk = &buf[..n];
        prefix.extend_from_slice(
            &chunk[..chunk
                .len()
                .min(NUL_SCAN_WINDOW.saturating_sub(prefix.len()))],
        );
        let decoded = decoder.decode(chunk, n == 0);
        for unit in decoded {
            let selecting = current_line >= offset && current_line < end_line;
            if unit == 10 {
                if selecting {
                    if line.last() == Some(&13) {
                        line.pop();
                        selected_bytes = selected_bytes.saturating_sub(1);
                    }
                    check_selected_range_size(
                        selected_bytes.saturating_add(selected.len()),
                        max_selected_bytes,
                    )?;
                    selected.push(std::mem::take(&mut line));
                }
                current_line = current_line.saturating_add(1);
            } else if selecting {
                selected_bytes = selected_bytes
                    .saturating_add(appended_unit_utf8_bytes(line.last().copied(), unit));
                check_selected_range_size(
                    // A terminal CR is removed at LF/EOF. Defer charging
                    // that single pending byte until another character makes
                    // it part of the selected content.
                    selected_bytes
                        .saturating_sub(usize::from(unit == 13))
                        .saturating_add(selected.len()),
                    max_selected_bytes,
                )?;
                line.push(unit);
            }
        }
        if n == 0 {
            break;
        }
    }
    if current_line >= offset && current_line < end_line {
        if line.last() == Some(&13) {
            line.pop();
            selected_bytes = selected_bytes.saturating_sub(1);
        }
        check_selected_range_size(
            selected_bytes.saturating_add(selected.len()),
            max_selected_bytes,
        )?;
        selected.push(line);
    }
    let not_text = if decoder.utf16le {
        None
    } else if prefix.starts_with(&[0xfe, 0xff]) {
        Some(crate::file_meta::NotText::Utf16Be)
    } else if prefix.contains(&0) {
        Some(crate::file_meta::NotText::Nul)
    } else if decoder.invalid_utf8 {
        Some(crate::file_meta::NotText::NotUtf8)
    } else {
        None
    };
    let units = selected.join(&10);
    Ok(StreamedTextRange {
        content: String::from_utf16_lossy(&units),
        units,
        total_lines: current_line,
        read_lines: selected.len() as u64,
        not_text,
    })
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
    cancel: &CancellationToken,
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
        if cancel.is_cancelled() {
            // The supervising read job turns cancellation into ToolError::Aborted.
            return Err("aborted".to_string());
        }
        let processed =
            crate::image_read::process_image_with_cancellation(jpeg, || cancel.is_cancelled())?;
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
    format_too_large_for_limit(size, MAX_FILE_READ_SIZE)
}

fn format_too_large_for_limit(size: u64, limit: u64) -> String {
    format!(
        "File content ({}) exceeds maximum allowed size ({}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.",
        format_file_size(size),
        format_file_size(limit),
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
    requested_path: &std::path::Path,
    live_cwd: &std::path::Path,
) -> String {
    let cwd = std::fs::canonicalize(live_cwd).unwrap_or_else(|_| live_cwd.to_path_buf());
    let mut message = format!(
        "File does not exist. {FILE_NOT_FOUND_CWD_NOTE} {}.",
        cwd.display()
    );
    // The cwd "dropped repo folder" suggestion takes PRECEDENCE over the
    // same-stem sibling. Both suffixes are the VERBATIM `" Did you mean {x}?"`.
    if let Some(cwd_suggestion) = suggest_path_under_cwd(requested_path, &cwd) {
        message.push_str(&format!(" Did you mean {cwd_suggestion}?"));
    } else if let Some(similar) = find_similar_file(requested_path) {
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

/// Legacy long-form unchanged-file text retained for transcript replay and
/// context filtering. Claude Code 2.1.322 no longer produces this result.
pub const FILE_UNCHANGED_STUB: &str = "File unchanged since last read. The content from the earlier Read tool_result in this conversation is still current — refer to that instead of re-reading.";

/// Legacy short-form unchanged-file text (U+2014 em dash), retained for replay.
pub const FILE_UNCHANGED_SHORT: &str = "Wasted call \u{2014} file unchanged since your last Read. Refer to that earlier tool_result instead.";

/// Legacy seeded-context stub prefix, recognized when reading older results.
pub const FILE_UNCHANGED_SEEDED_PREFIX: &str =
    "<system-reminder>This file is already in your context";

/// Render an older seeded-context result using its original model-visible path.
#[must_use]
pub fn format_file_unchanged_seeded(path: &std::path::Path) -> String {
    format!(
        "{FILE_UNCHANGED_SEEDED_PREFIX} (see \"Contents of {}\" above) and has not changed on disk. Use that content instead of re-reading.</system-reminder>",
        path.display()
    )
}

/// Recognize the three legacy unchanged-file result forms during replay and
/// context filtering. New Read invocations do not produce these forms.
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
const READ_PROMPT_LONG: &str = include_str!("templates/claude-code-2.1.319/read-long.md");

/// The SHORT Read prompt — byte-locked VERBATIM to claude-code `Yhi(e,…)`'s
/// `Dh(e)===true` branch (binary offset 195602702), served to current-gen
/// default models. Interpolations resolved as in [`READ_PROMPT_LONG`]:
///   * `${LQe}` => 2000;  `${n}` (maxSizeInstruction) => "";
///   * `${r}` (offsetInstruction) => `Khi`; `${t}` (lineFormat) => `VBr`;
///   * `${PQe()?…:""}` PDF fragment => INCLUDED;  `${qhi}` => appended.
/// The ellipsis in "(PNG, JPG, …)" is U+2026; the qhi em-dash is U+2014.
const READ_PROMPT_SHORT: &str = include_str!("templates/claude-code-2.1.319/read-lean.md");

const READ_PROMPT_SHORT_REWRITE: &str =
    include_str!("templates/claude-code-2.1.319/read-lean-rewrite.md");

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

/// Source-preserving counterpart of [`add_line_numbers`]. Each line keeps its
/// original UTF-16 units while generated line numbers and separators are added.
#[must_use]
pub fn add_line_numbers_source(
    content: &Utf16JsonProjection,
    start_line: u64,
) -> Utf16JsonProjection {
    let Some(units) = content.string_units("") else {
        return Utf16JsonProjection::from("");
    };
    if units.is_empty() {
        return Utf16JsonProjection::from("");
    }
    let mut output = Vec::with_capacity(units.len() + 16);
    for (offset, line) in units.split(|unit| *unit == b'\n' as u16).enumerate() {
        if offset > 0 {
            output.push(b'\n' as u16);
        }
        let line_number = start_line.saturating_add(offset as u64);
        output.extend(line_number.to_string().encode_utf16());
        output.push(b'\t' as u16);
        let line = line.strip_suffix(&[b'\r' as u16]).unwrap_or(line);
        output.extend_from_slice(line);
    }
    let display = String::from_utf16_lossy(&output);
    Utf16JsonProjection::root_string(display, output)
        .expect("display text is derived from the same UTF-16 units")
}

fn text_read_projection(
    file: &serde_json::Value,
    units: &[u16],
    not_text: Option<crate::file_meta::NotText>,
) -> (Utf16JsonProjection, Utf16JsonProjection) {
    let mut data = crate::generated_result(&json!({ "type": "text", "file": file }));
    data.set_string_units("/file/content", units)
        .expect("text content field");
    let mut model = map_read_result_text(&data).expect("text read result");
    if let Some(kind) = not_text {
        let mut model_units = model.string_units("").expect("model text");
        model_units.extend("\n\n".encode_utf16());
        model_units.extend(kind.read_warning().encode_utf16());
        model =
            Utf16JsonProjection::root_string(String::from_utf16_lossy(&model_units), model_units)
                .expect("matching display");
    }
    (data, model)
}

/// Re-run Read's model-text mapper for a result supplied by a Mod. The result
/// is already structured; this path must not read the file or update read state.
fn map_read_result_text(data: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    match data.string_at("/type")? {
        "text" => {
            let content = data.subprojection("/file/content").ok()?;
            let content_units = content.string_units("")?;
            let start = crate::unsigned_integer_at(data, "/file/startLine")?;
            let total = crate::unsigned_integer_at(data, "/file/totalLines")?;
            let count = crate::unsigned_integer_at(data, "/file/numLines")?;
            Some(if content_units.is_empty() {
                if count >= 1 && total > 1 {
                    format!("{start}\t").into()
                } else if count >= 1 || total == 0 {
                    EMPTY_FILE_WARNING.into()
                } else {
                    format_offset_beyond_eof(start, total).into()
                }
            } else {
                add_line_numbers_source(&content, start)
            })
        }
        "file_unchanged" => Some(if data.string_at("/source") == Some("seeded") {
            format_file_unchanged_seeded(std::path::Path::new(data.string_at("/file/filePath")?))
                .into()
        } else {
            FILE_UNCHANGED_SHORT.into()
        }),
        "notebook" => crate::notebook_read::render_cells_model_text_source(
            &data.subprojection("/file/cells").ok()?,
        ),
        "image" => Some("[Image content provided in tool result.]".into()),
        "pdf" => Some(
            format!(
                "PDF file read: {} ({})",
                data.string_at("/file/filePath")?,
                format_file_size(crate::unsigned_integer_at(data, "/file/originalSize")?)
            )
            .into(),
        ),
        "parts" => Some(
            format!(
                "PDF pages extracted: {} page(s) from {} ({})",
                crate::unsigned_integer_at(data, "/file/count")?,
                data.string_at("/file/filePath")?,
                format_file_size(crate::unsigned_integer_at(data, "/file/originalSize")?)
            )
            .into(),
        ),
        _ => None,
    }
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

fn check_read_route_policy(
    policy: &dyn lingxi_core::host::permission_gate::ReadPathPolicySnapshot,
    requested: &Path,
    translated: &Path,
    approved: &Path,
) -> Result<(), ToolError> {
    use lingxi_core::host::permission_gate::ReadPathPolicyMatch;

    for spelling in [requested, translated, approved] {
        let check = policy.check_path(spelling);
        if check.denied_by_read_rule != ReadPathPolicyMatch::NoMatch
            || ((spelling == translated || spelling == approved)
                && check.held_outside != ReadPathPolicyMatch::NoMatch)
            || policy.denied_search_path(spelling) != ReadPathPolicyMatch::NoMatch
        {
            return Err(ToolError::PathBlocked {
                path: requested.to_path_buf(),
            });
        }
    }
    Ok(())
}

struct LoadedRead {
    identity: Option<tool_api::read_file_state::ReadFileIdentity>,
    snapshot: lingxi_core::host::rooted_fs::RootedFileSnapshot,
    stream_source: Option<std::fs::File>,
}

// Encoded images need a physical input bound as well as the codec's decoded
// allocation limits. Keep the source ceiling aligned with notebook/PDF
// extraction snapshots; ordinary text retains its context-derived native cap.
#[cfg(feature = "image-read")]
const MAX_IMAGE_SOURCE_SIZE: u64 = 100 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ReadLoadPolicy {
    text_limit: u64,
    ranged: bool,
    pdf_pages: bool,
}

/// Read at most the admitted bytes plus one growth sentinel. This also bounds
/// sources that grow after their same-descriptor metadata was sampled.
fn read_admitted_bytes(
    source: &mut impl Read,
    maximum: u64,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, lingxi_core::host::rooted_fs::RootedFsError> {
    use lingxi_core::host::{rooted_fs::RootedFsError, FsError};
    let mut bytes = Vec::new();
    let mut chunk = [0; STREAMING_READ_CHUNK_BYTES];
    let ceiling = maximum.saturating_add(1);
    loop {
        if cancel.is_cancelled() {
            return Err(RootedFsError::Cancelled);
        }
        let remaining = ceiling.saturating_sub(bytes.len() as u64);
        if remaining == 0 {
            return Ok(bytes);
        }
        let length = remaining.min(chunk.len() as u64) as usize;
        match source.read(&mut chunk[..length]) {
            Ok(0) => return Ok(bytes),
            Ok(length) => bytes.extend_from_slice(&chunk[..length]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(RootedFsError::Fs(FsError::Io(error.to_string()))),
        }
    }
}

fn load_rooted_read(
    requested: &Path,
    approved: &Path,
    trusted_dirs: &[PathBuf],
    policy: ReadLoadPolicy,
    cancel: &CancellationToken,
) -> Result<LoadedRead, lingxi_core::host::rooted_fs::RootedFsError> {
    use lingxi_core::host::{rooted_fs::RootedFsError, FsError};
    use std::io::{Seek, SeekFrom};

    let mut file = open_rooted_file(requested, approved, trusted_dirs)?;
    let identity = tool_api::read_file_state::opened_read_file_identity(&file).ok();
    let metadata = file
        .metadata()
        .map_err(|error| RootedFsError::Fs(FsError::Io(error.to_string())))?;
    // The prefix is sufficient for extensionless image routing and bad-magic
    // diagnostics. Oversized inputs never need their complete body to reject.
    let mut prefix = [0; 16];
    let mut prefix_length = 0;
    while prefix_length < prefix.len() {
        if cancel.is_cancelled() {
            return Err(RootedFsError::Cancelled);
        }
        match file.read(&mut prefix[prefix_length..]) {
            Ok(0) => break,
            Ok(length) => prefix_length += length,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(RootedFsError::Fs(FsError::Io(error.to_string()))),
        }
    }
    #[cfg(feature = "image-read")]
    let image = is_image_path(requested)
        || (requested
            .extension()
            .is_none_or(|extension| extension.is_empty())
            && crate::image_read::sniff_image_media_type(&prefix[..prefix_length]).is_some());
    #[cfg(not(feature = "image-read"))]
    let image = false;
    let pdf = cfg!(feature = "pdf-read") && is_pdf_path(requested);
    let notebook = requested
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"));
    let rejected_binary =
        has_binary_extension(requested) && !is_image_path(requested) && !is_pdf_path(requested);
    let maximum = if rejected_binary {
        16
    } else if notebook {
        crate::notebook_read::MAX_NOTEBOOK_READ_SIZE
    } else if image {
        #[cfg(feature = "image-read")]
        {
            MAX_IMAGE_SOURCE_SIZE
        }
        #[cfg(not(feature = "image-read"))]
        {
            unreachable!()
        }
    } else if pdf {
        #[cfg(feature = "pdf-read")]
        {
            if policy.pdf_pages {
                crate::pdf_read::PDF_MAX_EXTRACT_SIZE
            } else {
                crate::pdf_read::PDF_TARGET_RAW_SIZE
            }
        }
        #[cfg(not(feature = "pdf-read"))]
        {
            unreachable!()
        }
    } else if policy.ranged {
        MAX_FILE_READ_SIZE
    } else {
        policy.text_limit
    };
    let _ = policy.pdf_pages;
    let stream = policy.ranged && !image && !pdf && !notebook && !rejected_binary;
    let mut size = metadata.len();
    let mut bytes = prefix[..prefix_length].to_vec();
    if size <= maximum {
        file.rewind()
            .map_err(|error| RootedFsError::Fs(FsError::Io(error.to_string())))?;
        bytes = read_admitted_bytes(&mut file, maximum, cancel)?;
        if bytes.len() as u64 > maximum {
            size = size
                .max(bytes.len() as u64)
                .max(file.metadata().map_or(0, |metadata| metadata.len()));
        }
    }
    let stream_source = if size > maximum && stream {
        file.seek(SeekFrom::Start(0))
            .map_err(|error| RootedFsError::Fs(FsError::Io(error.to_string())))?;
        bytes.clear();
        Some(file)
    } else {
        if size > maximum {
            // Later type-specific gates use the same metadata and this prefix
            // to retain their existing error messages and precedence.
            bytes.truncate(16);
        }
        None
    };
    Ok(LoadedRead {
        identity,
        snapshot: lingxi_core::host::rooted_fs::RootedFileSnapshot {
            bytes,
            size,
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            mode: Some(std::os::unix::fs::MetadataExt::mode(&metadata)),
            #[cfg(not(unix))]
            mode: None,
        },
        stream_source,
    })
}

fn check_read_cancelled(ctx: &ToolUseContext) -> Result<(), ToolError> {
    if ctx
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.is_cancelled())
    {
        Err(ToolError::Aborted)
    } else {
        Ok(())
    }
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

    fn read_path_policy_snapshot(
        &self,
    ) -> Arc<dyn lingxi_core::host::permission_gate::ReadPathPolicySnapshot> {
        self.ctx
            .permission_gate
            .as_ref()
            .map(|gate| gate.read_path_policy_snapshot())
            .unwrap_or_else(|| {
                permission::read_deny_globs::read_deny_search_snapshot(
                    self.ctx.permission_policy.clone(),
                    Some(&self.ctx.cwd()),
                )
            })
    }

    async fn emit_started(&self, invocation_id: &str, path: &std::path::Path) {
        self.ctx
            .bus
            .log_event_lazy(READ_STARTED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "_PROTO_file_path".to_string(),
                    AnalyticsValue::String(
                        PiiTagged::assert_pii_tagged_column(path.display().to_string())
                            .into_inner(),
                    ),
                );
                md
            })
            .await;
    }

    async fn emit_completed(&self, invocation_id: &str, bytes_read: u64, duration_ms: u64) {
        self.ctx
            .bus
            .log_event_lazy(READ_COMPLETED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "bytes_read".to_string(),
                    AnalyticsValue::Int(bytes_read as i64),
                );
                md.insert(
                    "duration_ms".to_string(),
                    AnalyticsValue::Int(duration_ms as i64),
                );
                md
            })
            .await;
    }

    async fn emit_failed(&self, invocation_id: &str, failure_kind: &str) {
        self.ctx
            .bus
            .log_event_lazy(READ_FAILED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "failure_kind".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(failure_kind.to_string()).into_inner(),
                    ),
                );
                md
            })
            .await;
    }

    /// Attribution for an existing read-state entry. Seeded context is tested
    /// first, followed by prior Read and mutation provenance. Every invocation
    /// still performs fresh I/O.
    async fn emit_file_read_reread(&self, prior_op: &'static str) {
        self.ctx
            .bus
            .log_event_lazy(FILE_READ_REREAD, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "priorOp".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(prior_op.to_string()).into_inner(),
                    ),
                );
                md
            })
            .await;
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
        self.ctx
            .bus
            .log_event_lazy(SESSION_FILE_READ, || {
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
                md
            })
            .await;
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
        self.ctx
            .bus
            .log_event_lazy(FILE_READ_LIMITS_OVERRIDE, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "hasMaxTokens".to_string(),
                    AnalyticsValue::Bool(has_max_tokens),
                );
                md.insert(
                    "hasMaxSizeBytes".to_string(),
                    AnalyticsValue::Bool(has_max_size_bytes),
                );
                md
            })
            .await;
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
        requested_path: &std::path::Path,
        err: &std::io::Error,
        cancel: &CancellationToken,
    ) -> Result<ToolCallResult, ToolError> {
        // Faithful NotFound gate: only ENOENT gets the cwd-note message (TS
        // rethrows every other errno). Structurally every caller is already on
        // the NotFound branch; this makes the contract explicit + bulletproof.
        if err.kind() != std::io::ErrorKind::NotFound {
            self.emit_failed(invocation_id, "io_metadata").await;
            return Err(ToolError::Io(err.to_string().into()));
        }
        self.emit_failed(invocation_id, "io_metadata").await;
        // `getCwd()` analog: the LIVE cwd (`Ct()`), realpath-resolved (matching
        // TS's already-resolved cwd) with a fallback to the unresolved path.
        let live_cwd = self.cwd_now();
        let requested_path = requested_path.to_path_buf();
        let message = run_read_job(cancel, move |_| {
            Ok(file_not_found_message(&requested_path, &live_cwd))
        })
        .await?;
        Err(ToolError::Io(message.into()))
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
        cancel: &CancellationToken,
    ) -> Result<ToolCallResult, ToolError> {
        // FT-09: `q_l`'s two pre-decode guards (cc-238.js @294617599), which run
        // BEFORE any decode/resize. Without them LingXi surfaced the `image`
        // crate's own text (`Image file is empty (0 bytes)` / `failed to decode
        // image: …`), which names neither the path nor what the bytes actually
        // are.
        if bytes.is_empty() {
            self.emit_failed(invocation_id, "image_empty").await;
            return Err(ToolError::Io(
                crate::image_read::format_image_empty(canon).into(),
            ));
        }
        if crate::image_read::sniff_image_media_type(&bytes).is_none() {
            self.emit_failed(invocation_id, "image_bad_magic").await;
            return Err(ToolError::Io(
                crate::image_read::format_image_bad_magic(canon, &bytes).into(),
            ));
        }
        if original_size > MAX_IMAGE_SOURCE_SIZE {
            self.emit_failed(invocation_id, "image_too_large").await;
            return Err(ToolError::Io(
                format!(
                    "Image file exceeds maximum allowed source size of {}.",
                    format_file_size(MAX_IMAGE_SOURCE_SIZE)
                )
                .into(),
            ));
        }

        let processed = match run_read_job(cancel, move |cancel| {
            crate::image_read::process_image_with_cancellation(bytes, || cancel.is_cancelled())
                .map_err(|error| ToolError::Io(error.into()))
        })
        .await
        {
            Ok(p) => p,
            Err(ToolError::Aborted) => return Err(ToolError::Aborted),
            Err(e) => {
                self.emit_failed(invocation_id, "image_process").await;
                return Err(e);
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
            data: crate::generated_result(&serde_json::json!({ "type": "image", "file": file })),
            // The image reaches the model INSIDE the tool_result content array
            // (the dispatch loop derives `content_blocks` from `data` — the port
            // of claude's result-mapper `case "image"`), so this text is
            // display-only (TUI/transcript); claude's wire form has no text.
            model_content: Some("[Image content provided in tool result.]".into()),
            new_messages,
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
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
        bytes: Vec<u8>,
        original_size: u64,
        first: u32,
        last: u32,
        started: Instant,
        cancel: &CancellationToken,
    ) -> Result<ToolCallResult, ToolError> {
        let rendered = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ToolError::Aborted),
            rendered = crate::pdf_render::render_pdf_pages(bytes, original_size, first, last) => rendered,
        };
        let page_jpegs = match rendered {
            Ok(p) => p,
            Err(e) => {
                self.emit_failed(invocation_id, e.telemetry_code()).await;
                return Err(ToolError::Io(
                    e.message(&canon.display().to_string()).into(),
                ));
            }
        };
        let payload = run_read_job(cancel, {
            let canon = canon.to_path_buf();
            move |cancel| {
                Ok(build_pages_payload(
                    &canon,
                    original_size,
                    page_jpegs,
                    cancel,
                ))
            }
        })
        .await?;
        let (sources, data, model_content) = match payload {
            Ok(v) => v,
            Err(e) => {
                self.emit_failed(invocation_id, "pdf_page_encode").await;
                return Err(ToolError::Io(e.into()));
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
            data: crate::generated_result(&data),
            model_content: Some(model_content.into()),
            new_messages: vec![msg],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
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
        cancel: &CancellationToken,
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
            return Err(ToolError::Io(
                format!("PDF file is empty: {}", canon.display()).into(),
            ));
        }
        if !crate::pdf_read::looks_like_pdf(&bytes) {
            self.emit_failed(invocation_id, "pdf_invalid").await;
            return Err(ToolError::Io(
                format!(
                    "File is not a valid PDF (missing %PDF- header): {}",
                    canon.display()
                )
                .into(),
            ));
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
                ).into()));
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
                ).into()));
            }
            #[cfg(feature = "pdf-render")]
            {
                return self
                    .read_pdf_pages_result(
                        invocation_id,
                        canon,
                        bytes,
                        original_size,
                        first,
                        last,
                        started,
                        cancel,
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
                    "Reading specific PDF pages requires page extraction, which is not yet available. Read the whole PDF (omit pages) if it is small.".to_string().into(),
                ));
            }
        }

        // Reject the physical snapshot before parsing or base64 allocation.
        // Oversized snapshots retain only their header for the earlier magic
        // and range diagnostics, never a partial document for delivery.
        if original_size > crate::pdf_read::PDF_TARGET_RAW_SIZE {
            self.emit_failed(invocation_id, "pdf_too_large").await;
            return Err(ToolError::Io(
                format!(
                    "PDF file exceeds maximum allowed size of {}.",
                    format_file_size(crate::pdf_read::PDF_TARGET_RAW_SIZE)
                )
                .into(),
            ));
        }

        // No `pages` ⇒ inline read. A parseable page count > 10 is too many to
        // read inline (TS: pdfinfo page-count gate; `None` skips the gate,
        // mirroring pdfinfo returning null).
        let (bytes, page_count) = run_read_job(cancel, move |_| {
            let page_count = pdf_page_count(&bytes);
            Ok((bytes, page_count))
        })
        .await?;
        if let Some(count) = page_count {
            if count > PDF_AT_MENTION_INLINE_THRESHOLD {
                self.emit_failed(invocation_id, "pdf_too_many_pages").await;
                return Err(ToolError::Io(format!(
                    "This PDF has {count} pages, which is too many to read at once. Use the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). Maximum {PDF_MAX_PAGES_PER_READ} pages per request."
                ).into()));
            }
        }

        // Unsupported model ⇒ refuse inline; tell the model to use a newer model
        // or the `pages` parameter (claude-code FileReadTool.ts:979-985).
        if !supported {
            self.emit_failed(invocation_id, "pdf_unsupported_model")
                .await;
            return Err(ToolError::Io(
                "Reading full PDFs is not supported with this model. Use a newer model (Sonnet 3.5 v2 or later), or use the pages parameter to read specific page ranges (e.g., pages: \"1-5\", maximum 20 pages per request). Page extraction requires poppler-utils: install with `brew install poppler` on macOS or `apt-get install poppler-utils` on Debian/Ubuntu.".to_string().into(),
            ));
        }
        let data = run_read_job(cancel, move |_| {
            Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
        })
        .await?;
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
            data: crate::generated_result(&serde_json::json!({
                "type": "pdf",
                "file": {
                    "filePath": canon.display().to_string(),
                    "base64": data,
                    "originalSize": original_size,
                },
            })),
            model_content: Some(model_content.into()),
            new_messages: vec![msg],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::parse(include_str!(
        "templates/claude-code-2.1.322/read-input-schema.json"
    ))
    .expect("pinned read input schema is valid JSON")
});

#[async_trait]
impl Tool for FileReadTool {
    fn supports_persistent_chat(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn read_state_request_path(&self, requested: &Path) -> PathBuf {
        crate::normalize_model_file_path(&requested.to_string_lossy(), &self.ctx.cwd())
    }

    fn read_state_io_path(&self, requested: &Path) -> Result<PathBuf, ToolError> {
        let requested = self.read_state_request_path(requested);
        if is_device_file(&requested.to_string_lossy()) {
            return Err(ToolError::PathBlocked { path: requested });
        }
        translate_model_path(&self.ctx.fs, requested, false)
            .map_err(|message| ToolError::InvalidInput(message.into()))
    }

    async fn open_file_for_compaction(
        &self,
        requested: &Path,
    ) -> Result<Option<std::fs::File>, ToolError> {
        let requested =
            crate::normalize_model_file_path(&requested.to_string_lossy(), &self.ctx.cwd());
        if is_device_file(&requested.to_string_lossy()) {
            return Err(ToolError::PathBlocked { path: requested });
        }
        let path = self.read_state_io_path(&requested)?;
        let trusted_dirs = self.ctx.read_roots().await;
        let approved = canonicalize_and_validate(&path, &trusted_dirs)
            .map_err(|_| ToolError::PathBlocked { path: path.clone() })?;
        let policy = self.read_path_policy_snapshot();
        check_read_route_policy(policy.as_ref(), &requested, &path, &approved)?;
        tokio::task::spawn_blocking(move || open_rooted_file(&path, &approved, &trusted_dirs))
            .await
            .map_err(|error| ToolError::Io(error.to_string().into()))?
            .map(Some)
            .map_err(|error| ToolError::Io(error.to_string().into()))
    }

    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("read files, images, PDFs, notebooks".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        crate::input_repair_metadata()
    }
    fn map_result_text(&self, result: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
        map_read_result_text(result)
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        // Byte-locked VERBATIM to claude-code `DESCRIPTION`
        // (`FileReadTool/prompt.ts:12`).
        "Read a file from the local filesystem.".to_string()
    }

    async fn prompt(&self, opts: &PromptOptions) -> Utf16JsonProjection {
        // The request's bundle owns style selection and Claude-only rewrites.
        if tool_api::prompt_uses_lean(opts) {
            if tool_api::prompt_uses_rewrite(opts) {
                return READ_PROMPT_SHORT_REWRITE.into();
            }
            READ_PROMPT_SHORT.into()
        } else {
            READ_PROMPT_LONG.into()
        }
    }

    fn get_path(&self, input: &Utf16JsonProjection) -> Option<PathBuf> {
        crate::input_string(input, "file_path").map(PathBuf::from)
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let invocation_id = ulid_or_uuid();
        let file_path = crate::input_string(&input, "file_path")
            .ok_or_else(|| ToolError::InvalidInput("file_path is required".into()))?;
        check_read_cancelled(&ctx)?;
        let read_cancel = ctx
            .cancel
            .as_ref()
            .map_or_else(CancellationToken::new, CancellationToken::child_token);
        let _cancel_read_on_drop = read_cancel.clone().drop_guard();
        // Native defaults offset to one before dispatch and state recording.
        // An absent limit reads to EOF; notebook dispatch resets both values.
        let input_offset = read_range_argument(&input, "offset", false)?;
        let input_limit = read_range_argument(&input, "limit", true)?;
        let offset = input_offset.unwrap_or(1);
        let allowance = ReadAllowance {
            room: ctx.options.read_context_room.as_deref().filter(|room| {
                room.matches_route(
                    &ctx.options.main_loop_model,
                    ctx.options.model_profile.as_deref(),
                )
            }),
            base: ReadLimits {
                max_size_bytes: MAX_FILE_READ_SIZE,
                max_tokens: effective_max_output_tokens(),
            },
            model_bytes_per_token: model_bytes_per_token(&ctx.options.main_loop_model),
            opted_in: crate::input_bool(&input, "allow_large").unwrap_or(false),
        };
        let initial_room = allowance.current_room();
        let raised = allowance.opted_in.then_some(initial_room.raised).flatten();
        let limits = raised.unwrap_or(allowance.base);

        let started = Instant::now();
        // Worktree parity plan (Task 3): a RELATIVE `file_path` resolves
        // against the CURRENT session cwd (`ctx.cwd()`, switchable by
        // `EnterWorktree`/`ExitWorktree`), not the frozen OS process cwd that
        // `std::fs::canonicalize` would otherwise consult below. An absolute
        // `file_path` (the documented/expected case) is unaffected.
        let requested_path = crate::checked_model_file_path(file_path, &self.ctx.cwd())?;
        // The scanner binds the exact JSON request. Match its original input
        // independently of trim/NFC/cwd normalization, and retain it for the
        // open (including the screenshot fallback decision below).
        let reminder_target = tool_api::read_file_state::reminder_read_target(Path::new(file_path))
            .map_err(|_| ToolError::PathBlocked {
                path: requested_path.clone(),
            })?;
        let is_notebook = requested_path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"));
        let offset = if is_notebook { 1 } else { offset };
        let input_limit = if is_notebook { None } else { input_limit };
        let limit = input_limit;
        let task_output_id = if let Some(registry) = self.ctx.task_registry.as_ref() {
            registry.task_output_directory().await.and_then(|root| {
                lingxi_core::host::task_output::output_id(Path::new(&root), &requested_path)
                    .map(str::to_owned)
            })
        } else {
            None
        };
        // Native `VE(file_path)` resolves against the current session cwd and
        // normalizes dot segments without following symlinks. Use that lexical
        // route as the read-state key and capture it beside any translated host
        // spelling. `canon` below remains the permission/I/O identity.
        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, requested_path.clone(), false) {
            Ok(path) => path,
            Err(message) => {
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::InvalidInput(message.into()));
            }
        };
        self.emit_started(&invocation_id, &requested_path).await;

        // #11: refuse blocking device/special files (claude-code `$3p` in
        // validateInput, BEFORE symlink resolution). Checked on the supplied
        // path string (device/proc paths are absolute, so no ~/cwd expansion is
        // needed). `${e}` is the original `file_path`.
        if is_device_file(requested_path.to_str().unwrap_or(file_path)) {
            self.emit_failed(&invocation_id, "device_file").await;
            return Err(ToolError::InvalidInput(format!(
                "Cannot read '{file_path}': this device file would block or produce infinite output."
            ).into()));
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
        let mut trusted_dirs = self.ctx.read_roots_for_tool(TOOL_NAME, &input).await;
        if let Some(root) =
            lingxi_core::host::teammate_plan::own_plan_file_root(ctx.agent_id.as_ref(), &path)
        {
            trusted_dirs.push(root);
        }

        let canonical = run_read_job(&read_cancel, {
            let path = path.clone();
            let trusted_dirs = trusted_dirs.clone();
            move |_| Ok(canonicalize_and_validate(&path, &trusted_dirs))
        })
        .await?;
        let mut canon = match canonical {
            Ok(p) => p,
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::PathBlocked { path });
            }
        };
        if reminder_target
            .as_ref()
            .is_some_and(|approved| approved != &canon)
        {
            self.emit_failed(&invocation_id, "path_blocked").await;
            return Err(ToolError::PathBlocked { path });
        }

        // The permission check above approved `canon`; all bytes and metadata
        // now come from a no-follow handle opened relative to the approved
        // trusted root. This is deliberately not `tokio::fs::read(&canon)`:
        // reopening the canonical pathname would reintroduce a parent/leaf
        // symlink retarget window.
        //
        // A ranged read retains the rooted descriptor and scans off-runtime;
        // all other reads load a fresh same-handle snapshot in the worker.
        let load_policy = ReadLoadPolicy {
            text_limit: limits.max_size_bytes,
            ranged: input_limit.is_some(),
            pdf_pages: crate::input_string(&input, "pages").is_some(),
        };
        let loaded = run_read_job(&read_cancel, {
            let path = path.clone();
            let canon = canon.clone();
            let trusted_dirs = trusted_dirs.clone();
            move |cancel| {
                Ok(load_rooted_read(
                    &path,
                    &canon,
                    &trusted_dirs,
                    load_policy,
                    cancel,
                ))
            }
        })
        .await?;
        let mut stream_source;
        let read_identity;
        let snapshot = match loaded {
            Ok(loaded) => {
                stream_source = loaded.stream_source;
                read_identity = loaded.identity;
                loaded.snapshot
            }
            Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
                lingxi_core::host::FsError::NotFound(_),
            )) => {
                // Missing-file UX (`FileReadTool.ts:608-649`). On ENOENT TS first
                // tries the macOS-screenshot AM/PM space variant (regular space ⇄
                // thin space, U+202F), then reports the friendly message if that
                // variant is also absent.
                if let Some(alt) = get_alternate_screenshot_path(&requested_path)
                    .filter(|_| reminder_target.is_none())
                {
                    // The original permission decision did not authorize this
                    // alternate filename. Recheck its current logical, guest
                    // backing and symlink landing routes before reading bytes.
                    let alt_path = self.read_state_io_path(&alt)?;
                    let policy = self.read_path_policy_snapshot();
                    let alternate = run_read_job(&read_cancel, {
                        let trusted_dirs = trusted_dirs.clone();
                        move |cancel| {
                            let Ok(alt_canon) = canonicalize_and_validate(&alt_path, &trusted_dirs)
                            else {
                                return Ok(None);
                            };
                            check_read_route_policy(policy.as_ref(), &alt, &alt_path, &alt_canon)?;
                            let snapshot = load_rooted_read(
                                &alt_path,
                                &alt_canon,
                                &trusted_dirs,
                                load_policy,
                                cancel,
                            );
                            Ok(Some((alt_canon, snapshot)))
                        }
                    })
                    .await?;
                    if let Some((alt_canon, snapshot)) = alternate {
                        match snapshot {
                            Ok(loaded) => {
                                canon = alt_canon;
                                stream_source = loaded.stream_source;
                                read_identity = loaded.identity;
                                loaded.snapshot
                            }
                            Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
                                lingxi_core::host::FsError::NotFound(_),
                            )) => {
                                let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
                                return self
                                    .file_not_found(
                                        &invocation_id,
                                        &requested_path,
                                        &not_found,
                                        &read_cancel,
                                    )
                                    .await;
                            }
                            Err(error) => {
                                self.emit_failed(&invocation_id, "io_metadata").await;
                                return Err(ToolError::Io(error.to_string().into()));
                            }
                        }
                    } else {
                        let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
                        return self
                            .file_not_found(
                                &invocation_id,
                                &requested_path,
                                &not_found,
                                &read_cancel,
                            )
                            .await;
                    }
                } else {
                    let not_found = std::io::Error::from(std::io::ErrorKind::NotFound);
                    return self
                        .file_not_found(&invocation_id, &requested_path, &not_found, &read_cancel)
                        .await;
                }
            }
            Err(lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged)
            | Err(lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged) => {
                self.emit_failed(&invocation_id, "symlink_resolution_changed")
                    .await;
                return Err(ToolError::InvalidInput(
                    symlink_resolution_changed_message(file_path).into(),
                ));
            }
            Err(lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile) if is_notebook => {
                self.emit_failed(&invocation_id, "notebook_not_regular_file")
                    .await;
                return Err(ToolError::Io(
                    crate::notebook_read::NOTEBOOK_NOT_REGULAR_FILE
                        .to_string()
                        .into(),
                ));
            }
            Err(error) => {
                self.emit_failed(&invocation_id, "io_metadata").await;
                return Err(ToolError::Io(error.to_string().into()));
            }
        };
        // Observe cancellation before parsing or publishing shared Read state.
        check_read_cancelled(&ctx)?;
        let size = snapshot.size;
        // Floor-truncated mtime in ms, matching TS `Math.floor(mtimeMs)` for
        // the read-state registry (`readFileState.set`). A missing mtime
        // (rare; e.g. platforms without mtime) falls back to the epoch (`0`).
        let mtime_ms = snapshot
            .modified
            .map(tool_api::read_file_state::mtime_ms_floor)
            .unwrap_or(0);

        // 2.1.322 always performs the read; existing state is retained only
        // for operation attribution. Legacy file_unchanged rows remain readable.
        if let Some(entry) =
            tool_api::read_file_state::get_metadata(&self.ctx.read_file_state, &requested_path)
        {
            self.emit_file_read_reread(if entry.seeded_from_context {
                "seeded"
            } else if entry.from_read {
                "read"
            } else {
                "edit_write"
            })
            .await;
        }

        // Image files route to the multimodal image path (claude-code routes by
        // extension to readImageWithTokenBudget, bypassing the text size cap).
        #[cfg(feature = "image-read")]
        let is_image = is_image_path(Path::new(file_path))
            || (Path::new(file_path)
                .extension()
                .is_none_or(|ext| ext.is_empty())
                && crate::image_read::sniff_image_media_type(
                    &snapshot.bytes[..snapshot.bytes.len().min(16)],
                )
                .is_some());
        #[cfg(not(feature = "image-read"))]
        let is_image = false;

        // PDF files route to the document path (claude-code routes by extension
        // to the PDF reader, which applies its own PDF size gates — 20MB inline /
        // 100MB extraction — not the 256KB text cap). Gated on the feature so the
        // cap still applies — and PDFs still hit the binary guard — when
        // `pdf-read` is off.
        let is_pdf = cfg!(feature = "pdf-read") && is_pdf_path(Path::new(file_path));

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
        if has_binary_extension(&requested_path)
            && !is_image_path(&requested_path)
            && !is_pdf_path(&requested_path)
        {
            self.emit_failed(&invocation_id, "binary_file").await;
            return Err(ToolError::Io(format_binary(&canon).into()));
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
        // Notebook source files have their own physical-size guard. Their
        // serialized cells, rather than the source bytes, receive the ordinary
        // text byte/token limits below.
        if is_notebook && size > crate::notebook_read::MAX_NOTEBOOK_READ_SIZE {
            self.emit_failed(&invocation_id, "notebook_too_large").await;
            return Err(ToolError::Io(
                crate::notebook_read::notebook_too_large_message().into(),
            ));
        }

        // TS applies the byte cap ONLY when no `limit` is supplied
        // (`readFileInRange(..., limit === undefined ? maxSizeBytes : undefined)`
        // — FileReadTool.ts:1026). A ranged read (offset+limit) of a >256KB file
        // must succeed and return just the requested lines, so the cap is gated
        // on `input_limit.is_none()`. A no-limit oversize read still errors with
        // the byte-locked template (fixture-pinned `error_template`).
        if !is_image
            && !is_pdf
            && !is_notebook
            && input_limit.is_none()
            && size > limits.max_size_bytes
        {
            self.emit_failed(&invocation_id, "file_too_large").await;
            return Err(ToolError::Io(
                format!(
                    "{}{}",
                    format_too_large_for_limit(size, limits.max_size_bytes),
                    allowance.hint(Some(size), None)
                )
                .into(),
            ));
        }

        let ext = std::path::Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let max_selected_bytes = allowance
            .base
            .max_tokens
            .saturating_mul(128)
            .max(raised.map_or(0, |limits| limits.max_size_bytes));

        if !is_image
            && !is_pdf
            && !is_notebook
            && input_limit.is_some()
            && size > MAX_FILE_READ_SIZE
        {
            let streamed = match run_read_job(&read_cancel, {
                let source = stream_source.take();
                let bytes = snapshot.bytes;
                let canon = canon.clone();
                let ext = ext.clone();
                move |cancel| {
                    if let Some(file) = source {
                        read_text_range_streaming(
                            file,
                            &canon,
                            offset,
                            input_limit.expect("checked is_some"),
                            ext.as_deref(),
                            max_selected_bytes,
                            cancel,
                        )
                    } else {
                        read_text_range_streaming(
                            bytes.as_slice(),
                            &canon,
                            offset,
                            input_limit.expect("checked is_some"),
                            ext.as_deref(),
                            max_selected_bytes,
                            cancel,
                        )
                    }
                }
            })
            .await
            {
                Ok(result) => result,
                Err(ToolError::Aborted) => return Err(ToolError::Aborted),
                Err(e) => {
                    self.emit_failed(&invocation_id, "streaming_range_read")
                        .await;
                    return Err(e);
                }
            };

            let not_text = streamed.not_text;
            let mut slice = streamed.content;
            let slice_units = streamed.units;
            let total_lines = streamed.total_lines;
            let line_range_start = offset;
            let read_lines = streamed.read_lines;
            let token_estimate = refined_read_tokens(
                &allowance,
                &slice_units,
                ext.as_deref(),
                offset,
                raised.is_some(),
                limits.max_tokens,
                &read_cancel,
            )
            .await?;
            let read_claim =
                match admit_read_tokens(&allowance, limits, raised.is_some(), token_estimate) {
                    Ok(claim) => claim,
                    Err(token_cap) => {
                        self.emit_failed(&invocation_id, "max_tokens_exceeded")
                            .await;
                        return Err(ToolError::Io(
                            format!(
                                "{}{}",
                                format_max_tokens_exceeded(token_estimate, token_cap),
                                allowance.hint(None, Some(token_estimate))
                            )
                            .into(),
                        ));
                    }
                };

            let duration_ms = started.elapsed().as_millis() as u64;
            self.emit_completed(&invocation_id, size, duration_ms).await;
            self.emit_session_file_read(
                &requested_path,
                total_lines,
                read_lines,
                size,
                slice.len() as u64,
                offset,
                input_limit,
            )
            .await;

            check_read_cancelled(&ctx)?;
            tool_api::read_file_state::set_from_opened_file(
                &self.ctx.read_file_state,
                requested_path.clone(),
                tool_api::read_file_state::ReadFileEntry {
                    content: slice.clone(),
                    mtime_ms,
                    offset: Some(offset),
                    limit: input_limit,
                    from_read: true,
                    seeded_from_context: false,
                    is_partial_view: false,
                },
                vec![requested_path.clone(), path.clone()],
                Some(slice_units.clone()),
                read_identity,
            );

            let mut file = json!({
                "filePath": file_path,
                "content": std::mem::take(&mut slice),
                "numLines": read_lines,
                "startLine": line_range_start,
                "totalLines": total_lines,
            });
            if let Some(id) = &task_output_id {
                file["taskId"] = json!(id);
            }
            let (data, model_content) = text_read_projection(&file, &slice_units, not_text);
            if let Some(claim) = read_claim {
                claim.commit();
            }
            return Ok(ToolCallResult {
                data,
                model_content: Some(model_content),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                subagent_hooks_fired: false,
                mcp_meta: None,
            });
        }

        let bytes = snapshot.bytes;

        #[cfg(feature = "image-read")]
        if is_image {
            let result = self
                .read_image_result(&invocation_id, &canon, bytes, size, started, &read_cancel)
                .await;
            check_read_cancelled(&ctx)?;
            if result.is_ok() {
                ctx.nested_memory_triggers.enqueue(requested_path.clone());
            }
            return result;
        }

        #[cfg(feature = "pdf-read")]
        if is_pdf {
            let pages = crate::input_string(&input, "pages").map(str::to_string);
            let result = self
                .read_pdf_result(
                    &invocation_id,
                    &canon,
                    bytes,
                    size,
                    pages,
                    &ctx.options.main_loop_model,
                    started,
                    &read_cancel,
                )
                .await;
            check_read_cancelled(&ctx)?;
            return result;
        }

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
        if is_notebook {
            let notebook_size = bytes.len() as u64;
            // Keep ECMAScript JSON strings in the source carrier throughout
            // parsing, projection and serialization. A serde Value boundary
            // rejects valid lone-surrogate escapes before cells can be read.
            let (cells, cells_json) = match run_read_job(&read_cancel, move |_| {
                let cells =
                    crate::notebook_read::read_notebook_source(&String::from_utf8_lossy(&bytes))
                        .map_err(|error| ToolError::Io(error.into()))?;
                let cells_json = cells
                    .to_json_string()
                    .map_err(|error| ToolError::Io(error.to_string().into()))?;
                Ok((cells, cells_json))
            })
            .await
            {
                Ok(result) => result,
                Err(ToolError::Aborted) => return Err(ToolError::Aborted),
                Err(error) => {
                    self.emit_failed(&invocation_id, "notebook_parse").await;
                    return Err(error);
                }
            };
            // Reuse one canonical cells JSON string for byte/token admission
            // and the read-state registry, as Native does.

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
                return Err(ToolError::Io(
                    format!(
                        "Notebook content ({}) exceeds maximum allowed size ({}). {}",
                        format_file_size(cells_json_bytes),
                        format_file_size(MAX_FILE_READ_SIZE),
                        crate::notebook_read::NOTEBOOK_JQ_GUIDANCE,
                    )
                    .into(),
                ));
            }

            // Token-budget gate on the cells JSON — TS runs
            // `validateContentTokens(cellsJson, ext, maxTokens)`
            // (`FileReadTool.ts:838`) on the notebook path too, after the
            // byte-size check and before recording state. `ext` is `"ipynb"`
            // (bytesPerToken 4).
            let notebook_units = cells_json.encode_utf16().collect::<Vec<_>>();
            let notebook_tokens = refined_read_tokens(
                &allowance,
                &notebook_units,
                Some("ipynb"),
                1,
                false,
                allowance.base.max_tokens,
                &read_cancel,
            )
            .await?;
            if notebook_tokens > allowance.base.max_tokens {
                let msg = format_max_tokens_exceeded(notebook_tokens, allowance.base.max_tokens);
                self.emit_failed(&invocation_id, "max_tokens_exceeded")
                    .await;
                return Err(ToolError::Io(msg.into()));
            }

            // Rejected notebooks never need their model-facing text rendered.
            let model_content = crate::notebook_read::render_cells_model_text_source(&cells)
                .expect("processed notebook cells are an exact source array");

            let duration_ms = started.elapsed().as_millis() as u64;
            self.emit_completed(&invocation_id, notebook_size, duration_ms)
                .await;

            // Native stores serialized cells, offset one and no line limit.
            check_read_cancelled(&ctx)?;
            tool_api::read_file_state::set_from_opened_file(
                &self.ctx.read_file_state,
                requested_path.clone(),
                tool_api::read_file_state::ReadFileEntry {
                    content: cells_json,
                    mtime_ms,
                    offset: Some(1),
                    limit: None,
                    from_read: true,
                    seeded_from_context: false,
                    is_partial_view: false,
                },
                vec![requested_path.clone(), path.clone()],
                None,
                read_identity,
            );

            ctx.nested_memory_triggers.enqueue(requested_path.clone());

            return Ok(ToolCallResult {
                // binary `{type:"notebook", file:{filePath, cells}}`; the rendered
                // cells text rides on `model_content`.
                data: Utf16JsonProjection::object(vec![
                    ("type".into(), "notebook".into()),
                    (
                        "file".into(),
                        Utf16JsonProjection::object(vec![
                            ("filePath".into(), file_path.into()),
                            ("cells".into(), cells),
                        ])
                        .expect("notebook result file"),
                    ),
                ])
                .expect("notebook result"),
                model_content: Some(model_content),
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                subagent_hooks_fired: false,
                mcp_meta: None,
            });
        }

        let not_text = crate::file_meta::classify_not_text(&bytes);
        let content_units = crate::file_meta::decode_native_text_units(&bytes);
        let content = String::from_utf16_lossy(&content_units);
        let selected = select_text_range_units(&content_units, offset, limit);
        let total_lines = selected.total_lines;
        let mut slice = selected.content;
        let mut slice_units = selected.units;
        let line_range_start = offset;
        let mut read_lines = selected.read_lines;

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
        let token_estimate = refined_read_tokens(
            &allowance,
            &slice_units,
            ext.as_deref(),
            offset,
            raised.is_some(),
            limits.max_tokens,
            &read_cancel,
        )
        .await?;
        let mut partial_note: Option<String> = None;
        // Mirror `validateContentTokens`'s early-pass band: skip when 0 or
        // `<= maxTokens/4`; otherwise the offline fallback is `effectiveCount ==
        // estimate`, over budget iff `estimate > maxTokens`.
        let read_claim =
            match admit_read_tokens(&allowance, limits, raised.is_some(), token_estimate) {
                Ok(claim) => claim,
                Err(token_cap) => {
                    let is_full_read = offset <= 1 && limit.is_none();
                    if is_full_read {
                        // Graceful truncation (`T=U, S=lineCount, v=S, R=note`).
                        let trunc = truncate_units_to_token_budget(
                            &slice_units,
                            token_estimate,
                            allowance.base.max_tokens,
                            total_lines,
                            &requested_path.display().to_string(),
                        );
                        slice = trunc.content;
                        slice_units = trunc.units;
                        read_lines = trunc.line_count;
                        let cap_label = if raised.is_some() && token_cap > allowance.base.max_tokens
                        {
                            format!("allow_large had room for {token_cap} tokens this turn")
                        } else {
                            format!("cap {}", allowance.base.max_tokens)
                        };
                        let note = trunc.note.replace(
                            &format!(" tokens, cap {}", allowance.base.max_tokens),
                            &format!(" tokens, {cap_label}"),
                        );
                        partial_note = Some(note.replace(
                            ". Do NOT",
                            &format!(".{} Do NOT", allowance.hint(None, Some(token_estimate))),
                        ));
                    } else {
                        self.emit_failed(&invocation_id, "max_tokens_exceeded")
                            .await;
                        return Err(ToolError::Io(
                            format!(
                                "{}{}",
                                format_max_tokens_exceeded(token_estimate, token_cap),
                                allowance.hint(None, Some(token_estimate))
                            )
                            .into(),
                        ));
                    }
                    None
                }
            };

        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, bytes.len() as u64, duration_ms)
            .await;

        // `tengu_session_file_read` (`FileReadTool.ts:1069-1083`) — fired after a
        // successful TEXT read. Rust finishes this sink await before committing
        // the token reservation and seen-state, so cancelling sink delivery
        // cannot publish a Read which never returned. NOT fired on notebooks,
        // images or PDFs. The byte/line counts mirror
        // `readFileInRange`'s return: `totalBytes = Buffer.byteLength(text)` →
        // full content byte length; `readBytes = R!==void 0 ? byteLen(T) : _` →
        // the (possibly truncated) slice's byte length; `readLines = S` → the
        // model-facing line count (truncation overwrites it). `offset` is the
        // defaulted offset; `limit` only when supplied. See
        // [`emit_session_file_read`] for the `ext` / `messageID` (omitted) /
        // session-flag handling.
        self.emit_session_file_read(
            &requested_path,
            total_lines,
            read_lines,
            content.len() as u64,
            slice.len() as u64,
            offset,
            input_limit,
        )
        .await;

        // Record the range-limited view at the final synchronous publication
        // boundary, together with its owned token claim. A partial first page
        // records its returned line count as `limit`; route aliases remain
        // separate from canonical rooted-I/O authority.
        check_read_cancelled(&ctx)?;
        tool_api::read_file_state::set_from_opened_file(
            &self.ctx.read_file_state,
            requested_path.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: slice.clone(),
                mtime_ms,
                offset: Some(offset),
                limit: if partial_note.is_some() {
                    Some(read_lines)
                } else {
                    input_limit
                },
                // Retain Read provenance for mutation freshness checks.
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
            vec![requested_path.clone(), path.clone()],
            Some(slice_units.clone()),
            read_identity,
        );

        ctx.nested_memory_triggers.enqueue(requested_path.clone());

        // Model-facing serialization (FILE.A). `content` above stays the RAW
        // slice — that is what the TUI renders (`emit_tool_result` payload). The
        // model instead sees `model_content`: cat -n line numbers for non-empty
        // reads, or the byte-locked empty / offset-beyond-EOF
        // `<system-reminder>` warning otherwise. This mirrors
        // claude-code's `FileReadTool` mapper (`FileReadTool.ts:692-714`), where
        // the model string diverges from the UI chrome.
        // claude-code FileRead result `data` (2.1.191): `{type, file:{…}}` — pure
        // metadata; the cat -n model render rides on `model_content`. The text
        // `file` is `{filePath, content, numLines, startLine, totalLines,
        // truncatedByTokenCap?}` — `truncatedByTokenCap` is added only on a
        // token-cap-shrunk read (binary spreads `{truncatedByTokenCap:!0}` last).
        let mut file = json!({
            "filePath": file_path,
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

        let (data, model_content) = text_read_projection(&file, &slice_units, not_text);
        if let Some(claim) = read_claim {
            claim.commit();
        }
        Ok(ToolCallResult {
            data,
            model_content: Some(model_content),
            new_messages,
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
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

    fn sparse_admission_snapshot(name: &str, header: &[u8], size: u64, ranged: bool) -> LoadedRead {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(header).unwrap();
        file.set_len(size).unwrap();
        drop(file);
        load_rooted_read(
            &path,
            &path,
            &[root],
            ReadLoadPolicy {
                text_limit: MAX_FILE_READ_SIZE,
                ranged,
                pdf_pages: false,
            },
            &CancellationToken::new(),
        )
        .unwrap()
    }

    #[test]
    fn admission_rejects_oversized_text_and_binary_without_loading_the_body() {
        for name in ["large.txt", "large.zip"] {
            let loaded =
                sparse_admission_snapshot(name, b"a text header\n", 8 * 1024 * 1024, false);
            assert_eq!(loaded.snapshot.size, 8 * 1024 * 1024);
            assert!(loaded.snapshot.bytes.len() <= 16);
            assert!(loaded.stream_source.is_none());
        }
    }

    #[test]
    fn admission_retains_large_ranged_text_as_a_rewound_descriptor() {
        let loaded = sparse_admission_snapshot("large.txt", b"first line\n", 8 * 1024 * 1024, true);
        assert!(loaded.snapshot.bytes.is_empty());
        let mut file = loaded.stream_source.unwrap();
        let mut prefix = [0; 11];
        file.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"first line\n");
    }

    #[test]
    fn admission_stops_growing_sources_at_the_cap_plus_one() {
        let mut source = std::io::Cursor::new(vec![b'x'; 128]);
        let bytes = read_admitted_bytes(&mut source, 32, &CancellationToken::new()).unwrap();
        assert_eq!(bytes, vec![b'x'; 33]);
        assert_eq!(
            source.position(),
            33,
            "must not consume the rest of a growing source"
        );
    }

    #[test]
    fn admission_observes_cancellation_between_bounded_chunks() {
        struct CancelAfterChunk(CancellationToken);
        impl Read for CancelAfterChunk {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                bytes.fill(b'x');
                self.0.cancel();
                Ok(bytes.len())
            }
        }
        let cancel = CancellationToken::new();
        let result = read_admitted_bytes(&mut CancelAfterChunk(cancel.clone()), 1_000_000, &cancel);
        assert!(matches!(
            result,
            Err(lingxi_core::host::rooted_fs::RootedFsError::Cancelled)
        ));
    }

    #[test]
    fn admission_rejects_oversized_notebook_before_json_loading() {
        let size = crate::notebook_read::MAX_NOTEBOOK_READ_SIZE + 1;
        let loaded = sparse_admission_snapshot("large.ipynb", b"{\"cells\":[]}", size, false);
        assert_eq!(loaded.snapshot.size, size);
        assert!(loaded.snapshot.bytes.len() <= 16);
    }

    #[cfg(not(feature = "image-read"))]
    #[tokio::test]
    async fn admission_retains_alternate_screenshot_stream_for_binary_detection() {
        use std::io::Write;
        let tmp = TempDir::new().unwrap();
        let requested = tmp.path().join("Screenshot 2024-01-01 at 3.04.05 PM.png");
        let alternate = get_alternate_screenshot_path(&requested).unwrap();
        let mut file = std::fs::File::create(alternate).unwrap();
        file.write_all(b"\x89PNG\r\n\x1a\n\0").unwrap();
        file.set_len(8 * 1024 * 1024).unwrap();
        drop(file);
        let (builtin, _) = make_ctx(&tmp);
        let result = FileReadTool::new(builtin)
            .call(
                crate::generated_result(&json!({"file_path":requested,"limit":1})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert!(crate::test_data(&result.data)["file"]["content"]
            .as_str()
            .unwrap()
            .contains("PNG"));
        assert!(result
            .model_content
            .unwrap()
            .string_at("")
            .unwrap()
            .contains("NUL bytes"));
    }

    #[cfg(feature = "pdf-read")]
    #[test]
    fn admission_rejects_oversized_pdf_before_parser_loading() {
        let size = crate::pdf_read::PDF_TARGET_RAW_SIZE + 1;
        let loaded = sparse_admission_snapshot("large.pdf", b"%PDF-1.4\n", size, false);
        assert_eq!(loaded.snapshot.size, size);
        assert!(loaded.snapshot.bytes.starts_with(b"%PDF-"));
        assert!(loaded.snapshot.bytes.len() <= 16);
    }

    #[cfg(feature = "image-read")]
    #[test]
    fn admission_bounds_image_sources_including_extensionless_images() {
        let size = MAX_IMAGE_SOURCE_SIZE + 1;
        for name in ["large.png", "large"] {
            let loaded = sparse_admission_snapshot(name, b"\x89PNG\r\n\x1a\n", size, true);
            assert_eq!(loaded.snapshot.size, size);
            assert!(loaded.snapshot.bytes.len() <= 16);
            assert!(loaded.stream_source.is_none());
        }
    }

    fn checked_source(
        source: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> (String, Vec<u16>) {
        let display = source
            .string_at("")
            .expect("error source is a root string")
            .to_owned();
        let units = source
            .string_units("")
            .expect("error source retains UTF-16 units");
        assert_eq!(String::from_utf16_lossy(&units), display);
        (display, units)
    }

    fn source_text(source: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> String {
        checked_source(source).0
    }

    use std::sync::Arc;
    use telemetry::{AnalyticsBus, InMemorySink};
    use tempfile::TempDir;
    use tool_api::test_support::{fresh_ctx, fresh_tx, make_dummy_fs};

    #[tokio::test]
    async fn native_text_and_notebook_reads_enqueue_the_invocation_context() {
        let tmp = TempDir::new().unwrap();
        let text = tmp.path().join("source.rs");
        let notebook = tmp.path().join("source.ipynb");
        std::fs::write(&text, "fn source() {}\n").unwrap();
        std::fs::write(&notebook, serde_json::to_vec(&json!({"cells":[{"cell_type":"code","source":["1+1"],"outputs":[],"metadata":{},"execution_count":null}],"metadata":{},"nbformat":4,"nbformat_minor":5})).unwrap()).unwrap();
        let (builtin, _) = make_ctx(&tmp);
        let cwd = builtin.cwd();
        let tool = FileReadTool::new(builtin);
        for (path, kind) in [(text, "text"), (notebook, "notebook")] {
            let context = fresh_ctx();
            let queue = context.nested_memory_triggers.clone();
            let result = tool
                .call(
                    crate::generated_result(&json!({"file_path":path})),
                    context,
                    fresh_tx(),
                )
                .await
                .unwrap();
            assert_eq!(crate::test_data(&result.data)["type"], kind);
            assert_eq!(
                queue.queued(),
                vec![crate::normalize_model_file_path(
                    path.to_str().unwrap(),
                    &cwd
                )]
            );
        }
    }

    #[cfg(feature = "image-read")]
    #[tokio::test]
    async fn native_image_enqueue_follows_successful_decode() {
        let tmp = TempDir::new().unwrap();
        let good = tmp.path().join("good.png");
        let bad = tmp.path().join("bad.png");
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        std::fs::write(&good, bytes).unwrap();
        std::fs::write(&bad, b"not an image").unwrap();
        let (builtin, _) = make_ctx(&tmp);
        let cwd = builtin.cwd();
        let tool = FileReadTool::new(builtin);
        for (path, success) in [(good, true), (bad, false)] {
            let context = fresh_ctx();
            let queue = context.nested_memory_triggers.clone();
            let result = tool
                .call(
                    crate::generated_result(&json!({"file_path":path})),
                    context,
                    fresh_tx(),
                )
                .await;
            assert_eq!(result.is_ok(), success);
            if success {
                assert_eq!(
                    queue.queued(),
                    vec![crate::normalize_model_file_path(
                        path.to_str().unwrap(),
                        &cwd
                    )]
                );
            } else {
                assert!(queue.queued().is_empty());
            }
        }
    }

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
        let map = ctx.read_file_state.clone();
        let route = crate::normalize_model_file_path(link.to_str().unwrap(), &ctx.cwd());
        let target_identity = std::fs::canonicalize(&target).unwrap();
        let tool = FileReadTool::new(ctx);

        let result = tool
            .call(
                crate::generated_result(&json!({ "file_path": link.to_string_lossy() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "approved"
        );
        assert_eq!(
            crate::test_data(&result.data)["file"]["filePath"],
            link.to_string_lossy().as_ref()
        );
        assert!(tool_api::read_file_state::get(&map, &route).is_some());
        assert!(
            tool_api::read_file_state::get(&map, &target_identity).is_none(),
            "a direct Read records the observed lexical route, not the target realpath"
        );
    }

    #[tokio::test]
    async fn relative_read_uses_session_cwd_and_normalized_lexical_state_key() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        let target = tmp.path().join("source.txt");
        std::fs::write(&target, "relative route\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let map = ctx.read_file_state.clone();
        let normalized = crate::normalize_model_file_path("nested/../source.txt", &ctx.cwd());
        assert_eq!(normalized, target);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(&json!({ "file_path": "nested/../source.txt" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["filePath"],
            "nested/../source.txt"
        );
        assert_eq!(
            tool_api::read_file_state::get(&map, &normalized)
                .unwrap()
                .content,
            "relative route\n"
        );
    }

    #[test]
    fn symlink_resolution_changed_message_is_byte_exact() {
        assert_eq!(
            symlink_resolution_changed_message("/tmp/link.txt"),
            "Refusing to read /tmp/link.txt: its symlink resolution changed after permission was checked. If a link in the working directory is being rewritten concurrently, stop that and retry."
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retarget_after_read_before_publication_does_not_authorize_unseen_file() {
        struct RetargetAtPublication {
            route: PathBuf,
            clipped: PathBuf,
            once: std::sync::atomic::AtomicBool,
        }
        #[async_trait::async_trait]
        impl telemetry::sink::AnalyticsSink for RetargetAtPublication {
            async fn log_event(&self, name: &str, _: telemetry::sink::LogEventMetadata) {
                if name == "tengu_session_file_read"
                    && self.once.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    std::fs::remove_file(&self.route).unwrap();
                    std::fs::hard_link(&self.clipped, &self.route).unwrap();
                }
            }
            async fn log_event_async(
                &self,
                name: &str,
                metadata: telemetry::sink::LogEventMetadata,
            ) {
                self.log_event(name, metadata).await;
            }
            fn name(&self) -> &str {
                "retarget-at-read-publication"
            }
        }
        for limit in [None, Some(100)] {
            let tmp = TempDir::new().unwrap();
            let route = tmp.path().join("route.txt");
            let clipped = tmp.path().join("clipped.txt");
            std::fs::write(&route, "the complete A snapshot").unwrap();
            std::fs::write(&clipped, "B prefix and its unseen suffix").unwrap();
            let (ctx, _) = make_ctx(&tmp);
            tool_api::read_file_state::mark_start_only(&ctx.read_file_state, clipped.clone());
            ctx.bus
                .attach_sink(Arc::new(RetargetAtPublication {
                    route: route.clone(),
                    clipped: clipped.clone(),
                    once: std::sync::atomic::AtomicBool::new(true),
                }))
                .await;
            let read = FileReadTool::new(ctx.clone());
            let mut input = json!({"file_path":route});
            if let Some(limit) = limit {
                input["limit"] = json!(limit);
            }
            let result = read
                .call(crate::generated_result(&input), fresh_ctx(), fresh_tx())
                .await
                .unwrap();
            assert!(crate::test_data(&result.data)["file"]["content"]
                .as_str()
                .unwrap()
                .contains("complete A"));
            assert!(tool_api::read_file_state::start_only_write_guard(
                &ctx.read_file_state,
                &route,
                &route
            )
            .is_some());
            let write = crate::write::FileWriteTool::new(ctx.clone());
            let replace =
                crate::generated_result(&json!({"file_path":route,"content":"replacement"}));
            assert!(write
                .call(replace.clone(), fresh_ctx(), fresh_tx())
                .await
                .is_err());
            assert_eq!(
                std::fs::read_to_string(&clipped).unwrap(),
                "B prefix and its unseen suffix"
            );
            read.call(
                crate::generated_result(&json!({"file_path":route})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
            write
                .call(replace.clone(), fresh_ctx(), fresh_tx())
                .await
                .unwrap();
            write.call(replace, fresh_ctx(), fresh_tx()).await.unwrap();
        }
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

    #[tokio::test]
    async fn reminder_matches_raw_path_before_normalization_and_rejects_other_requests() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().canonicalize().unwrap().join("source.txt");
        std::fs::write(&target, "approved content\n").unwrap();
        let raw = PathBuf::from(format!("{} \t", target.display()));
        let (context, _) = make_ctx(&tmp);
        let reader = FileReadTool::new(context);
        let input = crate::generated_result(&json!({"file_path":raw}));
        let read = tool_api::read_file_state::with_reminder_read_target(
            raw.clone(),
            target.clone(),
            reader.call(input.clone(), fresh_ctx(), fresh_tx()),
        )
        .await
        .unwrap();
        assert_eq!(
            read.data.string_at("/file/content"),
            Some("approved content\n")
        );
        let mismatched = tool_api::read_file_state::with_reminder_read_target(
            target.clone(),
            target.clone(),
            reader.call(input.clone(), fresh_ctx(), fresh_tx()),
        )
        .await;
        assert!(
            matches!(mismatched, Err(ToolError::PathBlocked { .. })),
            "a mismatched internal request may not silently become an explicit Read"
        );
        reader.call(input, fresh_ctx(), fresh_tx()).await.unwrap();
    }

    #[cfg(not(feature = "image-read"))]
    #[tokio::test]
    async fn reminder_binding_cannot_read_an_alternate_screenshot_target() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let requested = root.join("Screenshot 2026-11-12 at 9.00.00 AM.png");
        let alternate = get_alternate_screenshot_path(&requested).unwrap();
        std::fs::write(&alternate, "ALTERNATE_SECRET").unwrap();
        let (context, _) = make_ctx(&tmp);
        let map = context.read_file_state.clone();
        let tool = FileReadTool::new(context);
        let input = crate::generated_result(&json!({"file_path":requested}));
        let result = tool_api::read_file_state::with_reminder_read_target(
            requested.clone(),
            requested.clone(),
            tool.call(input.clone(), fresh_ctx(), fresh_tx()),
        )
        .await;
        assert!(
            result.is_err(),
            "internal Read may not switch to an unchecked alternate target"
        );
        assert!(tool_api::read_file_state::get(&map, &requested).is_none());
        let explicit = tool.call(input, fresh_ctx(), fresh_tx()).await.unwrap();
        assert!(
            explicit
                .data
                .string_at("/file/content")
                .unwrap()
                .contains("ALTERNATE_SECRET"),
            "ordinary explicit Read keeps its screenshot fallback"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_read_job_keeps_the_runtime_heartbeat_running() {
        let (started, ready) = tokio::sync::oneshot::channel();
        let (heartbeat, observed) = std::sync::mpsc::channel();
        let job = tokio::spawn(async move {
            run_read_job(&CancellationToken::new(), move |_| {
                let _ = started.send(());
                observed
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .map_err(|error| ToolError::Internal(error.to_string()))?;
                Ok("read finished")
            })
            .await
        });
        ready.await.expect("blocking job starts");
        tokio::task::yield_now().await;
        heartbeat.send(()).unwrap();
        assert_eq!(job.await.unwrap().unwrap(), "read finished");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_read_job_cancels_streaming_before_the_next_chunk() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct GatedReader {
            started: Option<tokio::sync::oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
            reads: Arc<AtomicUsize>,
        }
        impl Read for GatedReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let reads = self.reads.fetch_add(1, Ordering::SeqCst);
                if let Some(started) = self.started.take() {
                    let _ = started.send(());
                    self.release
                        .recv_timeout(std::time::Duration::from_secs(3))
                        .map_err(std::io::Error::other)?;
                }
                if reads >= 2 {
                    return Ok(0);
                }
                output.fill(b'x');
                Ok(output.len())
            }
        }

        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (finished, stopped) = tokio::sync::oneshot::channel();
        let reads = Arc::new(AtomicUsize::new(0));
        let reader = GatedReader {
            started: Some(started),
            release: gate,
            reads: reads.clone(),
        };
        let job = tokio::spawn(async move {
            run_read_job(&CancellationToken::new(), move |cancel| {
                let result = read_text_range_streaming(
                    reader,
                    Path::new("gated.txt"),
                    2,
                    1,
                    None,
                    64,
                    cancel,
                );
                let _ = finished.send(matches!(result, Err(ToolError::Aborted)));
                result
            })
            .await
        });
        ready.await.expect("first chunk is being read");
        job.abort();
        assert!(matches!(job.await, Err(error) if error.is_cancelled()));
        release.send(()).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(3), stopped)
                .await
                .expect("cancelled scanner returns")
                .unwrap(),
            "the scanner observes the dropped caller's cancellation token"
        );
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn offloaded_stream_preserves_utf16_bytes_and_total_lines() {
        let text = "\u{feff}α\r\nβ\nx\nlast";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let result = run_read_job(&CancellationToken::new(), move |cancel| {
            read_text_range_streaming(
                std::io::Cursor::new(bytes),
                Path::new("utf16.txt"),
                2,
                2,
                Some("txt"),
                64,
                cancel,
            )
        })
        .await
        .unwrap();
        assert_eq!(result.content.as_bytes(), "β\nx".as_bytes());
        assert_eq!(result.read_lines, 2);
        assert_eq!(result.total_lines, 4);
        assert!(result.not_text.is_none());
    }

    #[test]
    fn max_size_byte_locked() {
        assert_eq!(MAX_FILE_READ_SIZE, 262_144);
    }

    #[test]
    fn mod_replacement_uses_reads_model_text_mapper() {
        let text = serde_json::json!({
            "type": "text",
            "file": {"filePath":"/tmp/a.txt","content":"one\ntwo","numLines":2,"startLine":4,"totalLines":9}
        });
        assert_eq!(
            map_read_result_text(&crate::generated_result(&text))
                .and_then(|text| text.string_at("").map(str::to_owned))
                .as_deref(),
            Some("4\tone\n5\ttwo")
        );
        let empty = serde_json::json!({
            "type": "text",
            "file": {"filePath":"/tmp/empty.txt","content":"","numLines":0,"startLine":1,"totalLines":0}
        });
        assert_eq!(
            map_read_result_text(&crate::generated_result(&empty))
                .and_then(|text| text.string_at("").map(str::to_owned))
                .as_deref(),
            Some(EMPTY_FILE_WARNING)
        );
        let blank_line = serde_json::json!({
            "type": "text",
            "file": {"filePath":"/tmp/a.txt","content":"","numLines":1,"startLine":3,"totalLines":9}
        });
        assert_eq!(
            map_read_result_text(&crate::generated_result(&blank_line))
                .and_then(|text| text.string_at("").map(str::to_owned))
                .as_deref(),
            Some("3\t")
        );
        let unchanged = serde_json::json!({
            "type": "file_unchanged", "file":{"filePath":"/tmp/a.txt"}
        });
        assert_eq!(
            map_read_result_text(&crate::generated_result(&unchanged))
                .and_then(|text| text.string_at("").map(str::to_owned))
                .as_deref(),
            Some(FILE_UNCHANGED_SHORT)
        );
    }

    #[test]
    fn mod_read_mapper_preserves_lone_units_in_file_content() {
        let data = Utf16JsonProjection::parse(
            r#"{"type":"text","file":{"filePath":"/tmp/a.txt","content":"a\ud800b","numLines":1,"startLine":7,"totalLines":1}}"#,
        )
        .unwrap();
        let mapped = map_read_result_text(&data).expect("text result maps");
        assert_eq!(
            mapped.string_units(""),
            Some(
                "7\ta"
                    .encode_utf16()
                    .chain([0xd800])
                    .chain("b".encode_utf16())
                    .collect()
            )
        );
    }

    #[test]
    fn ranged_streaming_read_keeps_only_selected_lines() {
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
            &CancellationToken::new(),
        )
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
    fn streaming_empty_lines_enforce_selection_byte_budget() {
        for content in ["\n\n\n", "\r\n\r\n\r\n"] {
            let error = read_text_range_streaming(
                content.as_bytes(),
                Path::new("text.txt"),
                1,
                4,
                None,
                2,
                &CancellationToken::new(),
            )
            .err()
            .expect("three separators exceed two selected bytes");
            assert!(error
                .model_facing_message()
                .string_at("")
                .unwrap()
                .contains("over 2 bytes of text"));
        }
    }

    #[test]
    fn streaming_range_charges_normalized_crlf_bytes_at_the_cap() {
        for content in ["a\r\n", "a\r"] {
            let result = read_text_range_streaming(
                content.as_bytes(),
                Path::new("text.txt"),
                1,
                1,
                None,
                1,
                &CancellationToken::new(),
            )
            .expect("one normalized byte fits exactly");
            assert_eq!(result.content, "a");
        }
        assert!(
            read_text_range_streaming(
                "a\r\r\n".as_bytes(),
                Path::new("text.txt"),
                1,
                1,
                None,
                1,
                &CancellationToken::new()
            )
            .is_err(),
            "the earlier CR remains content and must be charged"
        );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "hello\nworld\n"
        );
        // FILE.4: TS counts the trailing newline's empty final fragment as a line
        // ("hello\nworld\n" => 3, matching readFileInRange `lineIndex`).
        assert_eq!(crate::test_data(&result.data)["file"]["totalLines"], 3);
        // Model-facing string: cat -n (compact tab format, 1-based from offset).
        // No cyber-risk reminder (parity verdict 12/14 — removed).
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
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
            .call(
                crate::generated_result(&json!({ "file_path": "rel.txt" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "in the worktree\n",
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
            .call(
                crate::generated_result(&json!({ "file_path": "rel.txt" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "at the boot cwd\n"
        );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = source_text(&err.model_facing_message());
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 2 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ranged read of an oversize file must succeed");
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "second\nthird"
        );
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 1, "limit": 1 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("over-budget single-line range must fail its token gate");
        let msg = source_text(&err.model_facing_message());
        assert_eq!(
            msg,
            format_max_tokens_exceeded(65_792, DEFAULT_MAX_OUTPUT_TOKENS)
        );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = source_text(&err.model_facing_message());
        assert!(msg.contains("exceeds maximum allowed size"), "got: {msg}");
        assert!(msg.contains("Use offset and limit"), "got: {msg}");
    }

    #[tokio::test]
    async fn extensionless_nul_content_returns_encoding_warning() {
        // Native returns the decoded view and warns that mutations will
        // refuse the NUL-bearing source; extension rejection is a separate gate.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("bin");
        let mut data = vec![b'A'; 100];
        data[10] = 0;
        std::fs::write(&target, &data).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"]
                .as_str()
                .unwrap()
                .as_bytes(),
            data
        );
        assert!(crate::test_model_text(result.model_content.as_ref())
            .unwrap()
            .ends_with(&crate::file_meta::NotText::Nul.read_warning()));
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        // The model-facing source preserves the exact tail; Display is only a logging label.
        assert!(
            source_text(&err.model_facing_message()).ends_with(
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("no-limit read must succeed");
        // ALL 2500 lines returned (no cap).
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"]
                .as_str()
                .unwrap()
                .lines()
                .count(),
            2500
        );
        assert!(crate::test_data(&result.data)["file"]["content"]
            .as_str()
            .unwrap()
            .ends_with("L2500\n"));
        // Both counts include the terminal empty line; content still has
        // exactly 2500 nonempty lines and no runtime line cap.
        assert_eq!(crate::test_data(&result.data)["file"]["startLine"], 1);
        assert_eq!(crate::test_data(&result.data)["file"]["numLines"], 2501);
        assert_eq!(crate::test_data(&result.data)["file"]["totalLines"], 2501);
        // NO truncation note of any kind.
        let mc = crate::test_model_text(result.model_content.as_ref()).unwrap();
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "limit": 2200 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("explicit-limit read must succeed");
        // 2200 lines returned (> 2000 — not clamped).
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"]
                .as_str()
                .unwrap()
                .lines()
                .count(),
            2200
        );
        let mc = crate::test_model_text(result.model_content.as_ref()).unwrap();
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
                &crate::generated_result(&json!({})),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert_eq!(desc, "Read a file from the local filesystem.");
        let prompt_source = tool
            .prompt(&PromptOptions {
                prompt_policy: None,
                is_scheduled_turn: false,
                session_id: None,
                include_examples: false,
                model: Some("claude-sonnet-4-6".into()),
                model_profile: None,
                ..Default::default()
            })
            .await;
        let prompt = prompt_source
            .string_at("")
            .expect("prompt is a JSON string");
        // model:claude-sonnet-4-6 selects the native long template prompt.
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
        assert!(prompt.contains(
            "When you already know which part of the file you need, only read that part. This can be important for larger files."
        ));
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
        let prompt_source = tool
            .prompt(&PromptOptions {
                prompt_policy: None,
                is_scheduled_turn: false,
                session_id: None,
                include_examples: false,
                model: Some("claude-opus-4-8".to_string()),
                model_profile: None,
                ..Default::default()
            })
            .await;
        let prompt = prompt_source
            .string_at("")
            .expect("prompt is a JSON string");
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 1, "limit": 1 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["file"]["content"], "line1");
        // cat -n numbers from offset=1. No cyber-risk reminder (verdict 12/14).
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
            "1\tline1"
        );
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 1 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["file"]["content"], "line2");
        // Numbering starts at the requested offset (2), not 1. No cyber-risk
        // reminder (verdict 12/14).
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
            "2\tline2"
        );
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 50 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["file"]["content"], "line2\n");
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "alpha\nbeta\ngamma\n"
        );
        // FILE.4: trailing newline => phantom final line ("...\n" => 4).
        assert_eq!(crate::test_data(&result.data)["file"]["totalLines"], 4);
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
        let key = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let tool = FileReadTool::new(ctx);
        tool.call(
            crate::generated_result(
                &json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 1 }),
            ),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let entry =
            tool_api::read_file_state::get(&map, &key).expect("registry entry recorded on read");
        // Content is the range-limited slice (TS stores `readFileInRange`'s
        // output), and offset/limit are the verbatim input values.
        assert_eq!(entry.content, "line2");
        assert_eq!(entry.offset, Some(2));
        assert_eq!(entry.limit, Some(1));
        // mtime recorded as a non-negative floor-truncated millisecond value.
        assert!(entry.mtime_ms >= 0);
    }

    #[tokio::test]
    async fn read_without_offset_limit_records_default_offset() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("b.txt");
        std::fs::write(&target, "alpha\nbeta\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let map = ctx.read_file_state.clone();
        let key = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let tool = FileReadTool::new(ctx);
        tool.call(
            crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let entry = tool_api::read_file_state::get(&map, &key).unwrap();
        assert_eq!(entry.content, "alpha\nbeta\n");
        assert_eq!(entry.offset, Some(1));
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["file"]["content"], "hello");
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
                crate::generated_result(&json!({ "file_path": link.to_str().unwrap() })),
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "hello from extra"
        );
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
                crate::generated_result(&json!({ "file_path": link.to_str().unwrap() })),
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

    #[tokio::test]
    async fn registered_skill_assets_are_readable_but_not_writable_or_escapable() {
        struct Roots(std::path::PathBuf);
        #[async_trait::async_trait]
        impl tool_api::builtin_context::SkillReadRoots for Roots {
            async fn roots(&self) -> Vec<std::path::PathBuf> {
                vec![self.0.clone()]
            }
        }
        let workspace = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let skill = home.path().join("skills/dataviz");
        std::fs::create_dir_all(skill.join("references")).unwrap();
        let reference = skill.join("references/choosing-a-form.md");
        std::fs::write(&reference, "Use a bar chart").unwrap();
        let secret = home.path().join("credentials.json");
        std::fs::write(&secret, "private").unwrap();
        std::os::unix::fs::symlink(&secret, skill.join("escape.md")).unwrap();
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(AnalyticsBus::new()),
            vec![workspace.path().to_path_buf()],
        );
        ctx.skill_read_roots = Some(Arc::new(Roots(skill.clone())));
        let read = FileReadTool::new(ctx.clone());
        read.call(
            crate::generated_result(&json!({"file_path":reference})),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let glob = crate::glob::GlobTool::new(ctx.clone());
        glob.call(
            crate::generated_result(&json!({"path":skill,"pattern":"**/*.md"})),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let grep = crate::grep::GrepTool::new(ctx.clone());
        grep.call(
            crate::generated_result(&json!({"path":reference,"pattern":"bar chart"})),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        for file in [secret, skill.join("escape.md")] {
            assert!(matches!(
                read.call(
                    crate::generated_result(&json!({"file_path":file})),
                    fresh_ctx(),
                    fresh_tx()
                )
                .await,
                Err(ToolError::PathBlocked { .. })
            ));
        }
        assert!(matches!(
            glob.call(
                crate::generated_result(&json!({"path":home.path(),"pattern":"**/*.md"})),
                fresh_ctx(),
                fresh_tx()
            )
            .await,
            Err(ToolError::PathBlocked { .. })
        ));
        let write = crate::write::FileWriteTool::new(ctx);
        assert!(matches!(
            write
                .call(
                    crate::generated_result(&json!({"file_path":reference,"content":"changed"})),
                    fresh_ctx(),
                    fresh_tx()
                )
                .await,
            Err(ToolError::PathBlocked { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(reference).unwrap(),
            "Use a bar chart"
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
                crate::generated_result(&json!({ "file_path": "/workspace/abc/note.txt" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "guest sees host bytes"
        );
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
                crate::generated_result(&json!({ "file_path": "/fenced/secret.txt" })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(message) => {
                assert!(
                    message.string_at("").unwrap().contains("not host-backed"),
                    "{}",
                    message.string_at("").unwrap()
                );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // TUI payload `content` stays the raw (empty) slice; the model sees the
        // empty-file warning instead of an empty string.
        assert_eq!(crate::test_data(&result.data)["file"]["content"], "");
        assert_eq!(crate::test_data(&result.data)["file"]["totalLines"], 1);
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
            EMPTY_FILE_WARNING
        );
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 10 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["file"]["content"], "");
        // FILE.4: "a\nb\nc\n" has 3 newlines => TS total_lines 4 (phantom final line).
        assert_eq!(crate::test_data(&result.data)["file"]["totalLines"], 4);
        assert_eq!(
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Notebook result variant: structured `cells`, NOT line-numbered text.
        let data = crate::test_data(&result.data);
        assert_eq!(data["type"], "notebook");
        let cells = data["file"]["cells"].as_array().unwrap();
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
            crate::test_model_text(result.model_content.as_ref()).unwrap(),
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
        let key = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        let tool = FileReadTool::new(ctx);
        tool.call(
            crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let entry = tool_api::read_file_state::get(&map, &key).expect("notebook read recorded");
        // Notebook reads record the default first line with no limit and
        // retain their Read provenance for the mutation freshness guard.
        assert_eq!(entry.offset, Some(1));
        assert_eq!(entry.limit, None);
        assert!(entry.from_read);
        // Stored content is the serialized cells JSON.
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            source_text(&err.model_facing_message()).contains("Notebook file is not valid JSON (it may be truncated, corrupted, or still being written):"),
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Io(m) => {
                assert_eq!(
                    m.string_at(""),
                    Some(crate::notebook_read::INVALID_NOTEBOOK_SHAPE)
                );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Io(m) => {
                assert_eq!(
                    m.string_at(""),
                    Some(crate::notebook_read::NOTEBOOK_NOT_REGULAR_FILE)
                );
            }
            other => panic!("expected Io with the not-a-regular-file message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn native_322_rereads_always_return_fresh_content_and_attribution() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/file_read_322.json"
        ))
        .unwrap();
        for case in fixture["rereadCases"].as_array().unwrap() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("file.txt");
            std::fs::write(&target, "fresh").unwrap();
            let (ctx, sink) = make_ctx(&tmp);
            ctx.bus.attach_sink(sink.clone()).await;
            if !case["input"]["entry"].is_null() {
                let input = &case["input"]["entry"];
                let mtime_ms = std::fs::metadata(&target)
                    .unwrap()
                    .modified()
                    .map(tool_api::read_file_state::mtime_ms_floor)
                    .unwrap();
                let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
                tool_api::read_file_state::set_with_model_context(
                    &ctx.read_file_state,
                    route,
                    tool_api::read_file_state::ReadFileEntry {
                        content: "prior".into(),
                        mtime_ms,
                        offset: input["offset"].as_u64(),
                        limit: input["limit"].as_u64(),
                        from_read: input["offset"].as_u64().is_some(),
                        seeded_from_context: input["seededFromContext"].as_bool().unwrap_or(false),
                        is_partial_view: input["isPartialView"].as_bool().unwrap_or(false),
                    },
                    true,
                );
            }
            let tool = FileReadTool::new(ctx);
            for input in [
                json!({"file_path":target}),
                json!({"file_path":target,"offset":1}),
            ] {
                let result = tool
                    .call(crate::generated_result(&input), fresh_ctx(), fresh_tx())
                    .await
                    .unwrap();
                let data = crate::test_data(&result.data);
                assert_eq!(data["type"], case["expected"]["data"]["type"]);
                assert_eq!(
                    data["file"]["content"],
                    case["expected"]["data"]["file"]["content"]
                );
                assert_eq!(
                    crate::test_model_text(result.model_content.as_ref()),
                    Some("1\tfresh")
                );
            }
            let events = sink.events().await;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.name == "tengu_session_file_read")
                    .count(),
                2
            );
            assert!(find_event(&events, "tengu_file_read_reread").is_some());
            assert!(find_event(&events, "tengu_file_read_dedup").is_none());
        }
    }

    #[tokio::test]
    async fn changed_file_reread_returns_updated_content() {
        use filetime::{set_file_mtime, FileTime};
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("changed.txt");
        std::fs::write(&target, "v0\n").unwrap();
        set_file_mtime(&target, FileTime::from_unix_time(1_000_000_000, 0)).unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        tool.call(
            crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Fresh content returned, not the stub.
        assert_eq!(crate::test_data(&again.data)["file"]["content"], "v1\n");
        assert_eq!(
            crate::test_data(&again.data)["type"],
            "text",
            "fresh read is a text result, not a dedup"
        );
    }

    #[tokio::test]
    async fn different_range_reread_returns_requested_line() {
        // First read full, then a ranged re-read of the same (unchanged) file:
        // each range is returned directly from the file.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("range.txt");
        std::fs::write(&target, "l1\nl2\nl3\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        tool.call(
            crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let ranged = tool
            .call(
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 1 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // Different range → real content, no stub.
        assert_eq!(crate::test_data(&ranged.data)["file"]["content"], "l2");
        assert_eq!(
            crate::test_data(&ranged.data)["type"],
            "text",
            "fresh read is a text result, not a dedup"
        );
    }

    #[tokio::test]
    async fn write_then_read_fetches_post_write_content() {
        // A Write records a post-write registry entry with `from_read=false`.
        // A subsequent Read fetches the on-disk result of the mutation.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("wr.txt");
        let (ctx, _sink) = make_ctx(&tmp);
        let map = ctx.read_file_state.clone();
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
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
            route,
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        // The fresh Read returns the post-write content.
        assert_eq!(crate::test_data(&result.data)["file"]["content"], "seed\n");
        assert_eq!(
            crate::test_data(&result.data)["type"],
            "text",
            "fresh read is a text result, not a dedup"
        );
    }

    // ───────────────────────── Token-budget gate ────────────────────────────

    #[test]
    fn rough_token_estimate_byte_locked() {
        // Round-half-up over UTF-16 units, bpt=4 for non-JSON: (len+2)/4.
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
    fn native_322_read_helper_fixtures() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/file_read_322.json"
        ))
        .unwrap();
        for case in fixture["tokenCases"].as_array().unwrap() {
            let input = &case["input"];
            assert_eq!(
                rough_token_count_estimation_for_file_type(
                    input["content"].as_str().unwrap(),
                    input["ext"].as_str(),
                ),
                case["expected"].as_u64().unwrap(),
                "{input}"
            );
        }
        for case in fixture["rangeCases"].as_array().unwrap() {
            let input = &case["input"];
            let expected = &case["expected"];
            let start = input["offset"].as_u64().unwrap();
            let selected = select_text_range(
                input["content"].as_str().unwrap(),
                start,
                input["limit"].as_u64(),
            );
            assert_eq!(
                selected.content,
                expected["content"].as_str().unwrap(),
                "{input}"
            );
            assert_eq!(
                selected.read_lines,
                expected["numLines"].as_u64().unwrap(),
                "{input}"
            );
            assert_eq!(
                selected.total_lines,
                expected["totalLines"].as_u64().unwrap(),
                "{input}"
            );
            assert_eq!(
                add_line_numbers(&selected.content, start),
                expected["numberedContent"].as_str().unwrap()
            );
            let result = crate::generated_result(&json!({
                "type": "text", "file": {"content": selected.content,
                    "numLines":selected.read_lines,"totalLines":selected.total_lines,
                    "startLine":start}
            }));
            assert_eq!(
                map_read_result_text(&result)
                    .unwrap()
                    .string_at("")
                    .unwrap(),
                expected["modelContent"].as_str().unwrap(),
                "{input}"
            );
        }
        for case in fixture["truncationCases"].as_array().unwrap() {
            let input = &case["input"];
            let expected = &case["expected"];
            let truncated = truncate_to_token_budget(
                input["content"].as_str().unwrap(),
                input["tokenCount"].as_u64().unwrap(),
                input["maxTokens"].as_u64().unwrap(),
                input["totalLines"].as_u64().unwrap(),
                input["filePath"].as_str().unwrap(),
            );
            assert_eq!(
                truncated.content,
                expected["content"].as_str().unwrap(),
                "{input}"
            );
            assert_eq!(
                truncated.line_count,
                expected["numLines"].as_u64().unwrap(),
                "{input}"
            );
            assert_eq!(
                truncated.note,
                expected["note"].as_str().unwrap(),
                "{input}"
            );
        }
        for case in fixture["roomCases"].as_array().unwrap() {
            let input = &case["input"];
            let room = tool_api::read_context_room::ReadContextRoom::new(
                input["threshold"].as_u64().unwrap(),
                input["held"].as_u64().unwrap(),
            );
            room.claim(input["claimed"].as_u64().unwrap());
            assert_eq!(
                room.room_tokens(input["usage"].as_u64().unwrap()),
                case["expected"].as_u64().unwrap(),
                "{input}"
            );
        }
        for case in fixture["allowanceCases"].as_array().unwrap() {
            let input = &case["input"];
            let expected = &case["expected"];
            let room = input["roomTokens"].as_u64().map(|tokens| {
                tool_api::read_context_room::ReadContextRoom::new(
                    tokens.saturating_mul(10).div_ceil(7),
                    0,
                )
            });
            let allowance = ReadAllowance {
                room: room.as_ref(),
                base: ReadLimits {
                    max_tokens: 25_000,
                    max_size_bytes: MAX_FILE_READ_SIZE,
                },
                opted_in: input["optedIn"].as_bool().unwrap(),
                model_bytes_per_token: model_bytes_per_token(input["model"].as_str().unwrap()),
            };
            assert_eq!(
                allowance.model_bytes_per_token,
                expected["modelBytesPerToken"].as_u64().unwrap()
            );
            let raised = allowance.current_room().raised.map(|limits|
                json!({"maxTokens":limits.max_tokens,"maxSizeBytes":limits.max_size_bytes}));
            assert_eq!(
                raised.unwrap_or(serde_json::Value::Null),
                expected["raised"],
                "{input}"
            );
            assert_eq!(
                allowance.hint(None, input["tokens"].as_u64()),
                expected["hint"].as_str().unwrap(),
                "{input}"
            );
        }
        for case in fixture["countCases"].as_array().unwrap() {
            let input = &case["input"];
            assert_eq!(
                count_read_tokens(
                    input["content"].as_str().unwrap(),
                    Some("txt"),
                    input["offset"].as_u64().unwrap(),
                    input["raised"].as_bool().unwrap()
                ),
                case["expected"].as_u64().unwrap(),
                "{input}"
            );
        }
        for case in fixture["promptCases"].as_array().unwrap() {
            let expected = match (
                case["input"]["lean"].as_bool().unwrap(),
                case["input"]["rewrite"].as_bool().unwrap(),
            ) {
                (false, _) => READ_PROMPT_LONG,
                (true, false) => READ_PROMPT_SHORT,
                (true, true) => READ_PROMPT_SHORT_REWRITE,
            };
            assert_eq!(expected, case["expected"].as_str().unwrap());
        }
        assert_eq!(
            MAX_FILE_READ_SIZE,
            fixture["schemaCase"]["maxSizeBytes"].as_u64().unwrap()
        );
        assert_eq!(
            INPUT_SCHEMA.string_at("/properties/allow_large/description"),
            fixture["schemaCase"]["allowLargeDescription"].as_str()
        );
    }

    #[tokio::test]
    async fn native_322_allow_large_uses_shared_trusted_room_and_unavailable_guidance() {
        let _env = READ_LIMIT_ENV_MUTEX.lock().await;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("large.txt");
        let body = "x".repeat(300_000);
        std::fs::write(&target, &body).unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let room = Arc::new(tool_api::read_context_room::ReadContextRoom::new(
            600_000, 0,
        ));
        let mut call = fresh_ctx();
        call.options.main_loop_model = "claude-opus-5".into();
        call.options.read_context_room = Some(room.clone());
        let ordinary = tool
            .call(
                crate::generated_result(&json!({"file_path":target})),
                call.clone(),
                fresh_tx(),
            )
            .await
            .err()
            .unwrap();
        assert!(ordinary
            .model_facing_message()
            .string_at("")
            .unwrap()
            .ends_with(ALLOW_LARGE_HINT));
        for _ in 0..3 {
            let result = tool
                .call(
                    crate::generated_result(&json!({"file_path":target,"allow_large":true})),
                    call.clone(),
                    fresh_tx(),
                )
                .await
                .unwrap();
            let data = crate::test_data(&result.data);
            assert_eq!(data["file"]["content"], body);
            assert!(data["file"].get("truncatedByTokenCap").is_none());
        }
        assert!(room.claimed() > 450_000);
        let partial = tool
            .call(
                crate::generated_result(&json!({"file_path":target,"allow_large":true})),
                call,
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&partial.data)["file"]["truncatedByTokenCap"],
            true
        );
        assert_eq!(partial.new_messages.len(), 1);
        let unavailable = tool
            .call(
                crate::generated_result(&json!({"file_path":target,"allow_large":true})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .err()
            .unwrap();
        assert!(unavailable
            .model_facing_message()
            .string_at("")
            .unwrap()
            .ends_with(ALLOW_LARGE_UNAVAILABLE_HINT));
    }

    #[test]
    fn native_322_lossless_utf16_range_and_truncation() {
        let fixture =
            Utf16JsonProjection::parse(include_str!("fixtures/utf16_read_native_322.json"))
                .unwrap();
        for index in 0..fixture.array_len("/rangeCases").unwrap() {
            let prefix = format!("/rangeCases/{index}");
            let content = fixture
                .string_units(&format!("{prefix}/input/content"))
                .unwrap();
            let start = fixture
                .number_at(&format!("{prefix}/input/offset"))
                .unwrap() as u64;
            let limit = fixture
                .number_at(&format!("{prefix}/input/limit"))
                .map(|value| value as u64);
            let selected = select_text_range_units(&content, start, limit);
            assert_eq!(
                selected.units,
                fixture
                    .string_units(&format!("{prefix}/expected/content"))
                    .unwrap(),
                "case {index}"
            );
            assert_eq!(
                selected.read_lines,
                fixture
                    .number_at(&format!("{prefix}/expected/numLines"))
                    .unwrap() as u64
            );
            assert_eq!(
                selected.total_lines,
                fixture
                    .number_at(&format!("{prefix}/expected/totalLines"))
                    .unwrap() as u64
            );
            let file = json!({"content":selected.content,"numLines":selected.read_lines,"totalLines":selected.total_lines,"startLine":start});
            let (_, model) = text_read_projection(&file, &selected.units, None);
            assert_eq!(
                model.string_units(""),
                fixture.string_units(&format!("{prefix}/expected/modelContent")),
                "case {index}"
            );
        }
        for index in 0..fixture.array_len("/truncationCases").unwrap() {
            let prefix = format!("/truncationCases/{index}");
            let content = fixture
                .string_units(&format!("{prefix}/input/content"))
                .unwrap();
            let truncation = truncate_units_to_token_budget(
                &content,
                fixture
                    .number_at(&format!("{prefix}/input/tokenCount"))
                    .unwrap() as u64,
                fixture
                    .number_at(&format!("{prefix}/input/maxTokens"))
                    .unwrap() as u64,
                fixture
                    .number_at(&format!("{prefix}/input/totalLines"))
                    .unwrap() as u64,
                fixture
                    .string_at(&format!("{prefix}/input/filePath"))
                    .unwrap(),
            );
            assert_eq!(
                truncation.units,
                fixture
                    .string_units(&format!("{prefix}/expected/content"))
                    .unwrap(),
                "case {index}"
            );
            assert_eq!(
                truncation.note,
                fixture
                    .string_at(&format!("{prefix}/expected/note"))
                    .unwrap(),
                "case {index}"
            );
            assert_eq!(
                truncation.line_count,
                fixture
                    .number_at(&format!("{prefix}/expected/numLines"))
                    .unwrap() as u64
            );
        }
    }

    #[test]
    fn streaming_utf16_retains_surrogates_across_every_byte_boundary() {
        struct ByteReader<'a>(&'a [u8]);
        impl Read for ByteReader<'_> {
            fn read(&mut self, destination: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() || destination.is_empty() {
                    return Ok(0);
                }
                destination[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let units = vec![0xfeff, 0xd800, 97, 0xdc00, 13, 10, 0xd83d, 0xde00, 0xd801];
        let mut bytes: Vec<u8> = units
            .iter()
            .flat_map(|unit: &u16| unit.to_le_bytes())
            .collect();
        bytes.push(0xff);
        let streamed = read_text_range_streaming(
            ByteReader(&bytes),
            Path::new("surrogates.txt"),
            1,
            3,
            Some("txt"),
            1024,
            &CancellationToken::new(),
        )
        .unwrap();
        let buffered = select_text_range_units(&units, 1, Some(3));
        assert_eq!(streamed.units, buffered.units);
        assert_eq!(streamed.content, buffered.content);
        assert_eq!(streamed.total_lines, buffered.total_lines);
        assert!(streamed.not_text.is_none());
        assert!(read_text_range_streaming(
            ByteReader(&bytes),
            Path::new("surrogates.txt"),
            2,
            1,
            Some("txt"),
            7,
            &CancellationToken::new()
        )
        .is_ok());
        assert!(read_text_range_streaming(
            ByteReader(&bytes),
            Path::new("surrogates.txt"),
            2,
            1,
            Some("txt"),
            6,
            &CancellationToken::new()
        )
        .is_err());
    }

    #[tokio::test]
    async fn reads_return_and_cache_exact_utf16_in_buffered_and_streaming_paths() {
        let _env = READ_LIMIT_ENV_MUTEX.lock().await;
        let tmp = TempDir::new().unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let state = ctx.read_file_state.clone();
        let tool = FileReadTool::new(ctx);
        let expected = vec![0xd800, 97, 0xdc00, 0xd83d, 0xde00, 0xd801];
        for streaming in [false, true] {
            let target = tmp.path().join(if streaming {
                "streamed-surrogates.txt"
            } else {
                "buffered-surrogates.txt"
            });
            let mut units = vec![0xfeff];
            let offset = if streaming {
                units.extend(std::iter::repeat_n([120, 10], 70_000).flatten());
                70_001
            } else {
                1
            };
            units.extend_from_slice(&expected);
            let mut bytes: Vec<u8> = units.into_iter().flat_map(u16::to_le_bytes).collect();
            bytes.push(0xff);
            std::fs::write(&target, bytes).unwrap();
            let result = tool
                .call(
                    crate::generated_result(&json!({"file_path":target,"offset":offset,"limit":1})),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap();
            assert_eq!(
                result.data.string_units("/file/content"),
                Some(expected.clone())
            );
            let mut model: Vec<u16> = format!("{offset}\t").encode_utf16().collect();
            model.extend_from_slice(&expected);
            assert_eq!(result.model_content.unwrap().string_units(""), Some(model));
            assert_eq!(
                tool_api::read_file_state::get_content_units(&state, &target),
                Some(expected.clone())
            );
        }
    }

    #[tokio::test]
    async fn native_322_disk_reads_preserve_range_and_encoding_contracts() {
        let _env = READ_LIMIT_ENV_MUTEX.lock().await;
        let tmp = TempDir::new().unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        for (name, bytes, expected, warning) in [
            ("cr.txt", b"a\r\r\n".to_vec(), "a\r\n", false),
            ("utf16.txt", vec![0xff, 0xfe, b'a', 0, 10, 0], "a\n", false),
            ("nul.txt", vec![b'a', 0, b'b'], "a\0b", true),
            ("legacy.txt", vec![b'a', 0xff, b'b'], "a\u{fffd}b", true),
        ] {
            let target = tmp.path().join(name);
            std::fs::write(&target, bytes).unwrap();
            let result = tool
                .call(
                    crate::generated_result(&json!({"file_path":target})),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap();
            assert_eq!(crate::test_data(&result.data)["file"]["content"], expected);
            assert_eq!(
                crate::test_model_text(result.model_content.as_ref())
                    .unwrap()
                    .contains("Edit and Write will refuse it"),
                warning
            );
        }
        let target = tmp.path().join("cjk.txt");
        let content = "中".repeat(80_000);
        std::fs::write(&target, &content).unwrap();
        let result = tool
            .call(
                crate::generated_result(&json!({"file_path":target})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["file"]["content"], content);
        assert!(
            result.new_messages.is_empty(),
            "UTF-8 bytes must not inflate native token estimate"
        );
        let target = tmp.path().join("range.txt");
        std::fs::write(&target, "中文\n".repeat(50_000)).unwrap();
        let result = tool
            .call(
                crate::generated_result(&json!({"file_path":target,"limit":30_000})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["file"]["numLines"], 30_000);
        assert!(!crate::test_data(&result.data)["file"]["content"]
            .as_str()
            .unwrap()
            .ends_with('\n'));
    }

    #[tokio::test]
    async fn native_322_normalized_notebook_and_raw_token_extension_routes() {
        let _env = READ_LIMIT_ENV_MUTEX.lock().await;
        let tmp = TempDir::new().unwrap();
        let notebook = tmp.path().join("book.ipynb");
        std::fs::write(&notebook, sample_ipynb()).unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let result = tool
            .call(
                crate::generated_result(
                    &json!({"file_path":format!(" {} ",notebook.display()),"offset":99,"limit":1}),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "notebook");
        let json_file = tmp.path().join("data.json");
        std::fs::write(&json_file, "中".repeat(70_000)).unwrap();
        let result = tool
            .call(
                crate::generated_result(
                    &json!({"file_path":format!(" {} ",json_file.display()),"limit":1}),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            70_000
        );
        assert!(tool
            .call(
                crate::generated_result(&json!({"file_path":json_file,"limit":1})),
                fresh_ctx(),
                fresh_tx()
            )
            .await
            .is_err());
    }

    #[cfg(feature = "image-read")]
    #[tokio::test]
    async fn native_322_image_dispatch_uses_raw_extension_or_extensionless_magic() {
        let tmp = TempDir::new().unwrap();
        let named = tmp.path().join("image.png");
        std::fs::write(&named, "plain text").unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let tool = FileReadTool::new(ctx);
        let plain = tool
            .call(
                crate::generated_result(&json!({"file_path":format!(" {} ",named.display())})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&plain.data)["type"], "text");
        let extensionless = tmp.path().join("image");
        let image = image::DynamicImage::new_rgb8(2, 2);
        let mut png = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut png, image::ImageOutputFormat::Png)
            .unwrap();
        std::fs::write(&extensionless, png.into_inner()).unwrap();
        let decoded = tool
            .call(
                crate::generated_result(&json!({"file_path":extensionless})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&decoded.data)["type"], "image");
    }

    #[cfg(feature = "pdf-read")]
    #[tokio::test]
    async fn native_322_pdf_dispatch_uses_raw_extension() {
        let tmp = TempDir::new().unwrap();
        let named = tmp.path().join("document.pdf");
        std::fs::write(&named, "plain text").unwrap();
        let (ctx, _) = make_ctx(&tmp);
        let result = FileReadTool::new(ctx)
            .call(
                crate::generated_result(&json!({"file_path":format!(" {} ",named.display())})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(crate::test_data(&result.data)["type"], "text");
    }

    #[test]
    fn native_322_streamed_ranges_match_buffered_range_fixtures() {
        struct ChunkReader<'a> {
            data: &'a [u8],
            chunk: usize,
        }
        impl Read for ChunkReader<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = output.len().min(self.chunk).min(self.data.len());
                output[..count].copy_from_slice(&self.data[..count]);
                self.data = &self.data[count..];
                Ok(count)
            }
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/file_read_322.json"
        ))
        .unwrap();
        for case in fixture["rangeCases"].as_array().unwrap() {
            let input = &case["input"];
            let expected = &case["expected"];
            let content = input["content"].as_str().unwrap();
            let bommed = if content.starts_with('\u{feff}') {
                content.to_string()
            } else {
                format!("\u{feff}{content}")
            };
            let utf16: Vec<u8> = bommed.encode_utf16().flat_map(u16::to_le_bytes).collect();
            for bytes in [content.as_bytes(), utf16.as_slice()] {
                for chunk in [1, 3, 7, STREAMING_READ_CHUNK_BYTES] {
                    let selected = read_text_range_streaming(
                        ChunkReader { data: bytes, chunk },
                        Path::new("/workspace/text.txt"),
                        input["offset"].as_u64().unwrap(),
                        input["limit"].as_u64().unwrap_or(u64::MAX),
                        Some("txt"),
                        DEFAULT_MAX_OUTPUT_TOKENS,
                        &CancellationToken::new(),
                    )
                    .unwrap();
                    assert_eq!(
                        selected.content,
                        expected["content"].as_str().unwrap(),
                        "{input}; chunk={chunk}"
                    );
                    assert_eq!(
                        selected.read_lines,
                        expected["numLines"].as_u64().unwrap(),
                        "{input}; chunk={chunk}"
                    );
                    assert_eq!(
                        selected.total_lines,
                        expected["totalLines"].as_u64().unwrap(),
                        "{input}; chunk={chunk}"
                    );
                    assert!(selected.not_text.is_none(), "valid text must not warn");
                }
            }
        }
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
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

        let key = crate::normalize_model_file_path(target.to_str().unwrap(), tmp.path());
        let entry = tool_api::read_file_state::get(&map, &key).expect("the read recorded an entry");
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("over-budget full read must gracefully truncate, not error");
        let mc = crate::test_model_text(result.model_content.as_ref()).unwrap();
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), tmp.path());
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
                        lingxi_core::types::ContentBlock::Text { text, .. } => Some(text.clone()),
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
                route.display()
            )),
            "the notice must carry the partial-view banner WITH the full path: {banner}"
        );
        assert!(banner.contains(" total ("));
        assert!(banner.contains(" tokens, cap 25000). Call Read with offset="));
        assert!(banner.contains(" or Grep to find a specific section."));
        assert!(banner.contains("Do NOT answer from this page alone"));
        // The returned slice is strictly smaller than the whole file.
        let returned_lines = crate::test_data(&result.data)["file"]["content"]
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "limit": 50_000 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = source_text(&err.model_facing_message());
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "offset": 1, "limit": 2 }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("small slice of a huge file passes the token gate");
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "first\nsecond"
        );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "hello\nworld\n"
        );
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
        // A normal text read fires tengu_session_file_read
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            crate::test_data(&result.data)["file"]["content"],
            "alpha\nbeta\ngamma\n"
        );

        let events = sink.events().await;
        let ev = find_event(&events, "tengu_session_file_read")
            .expect("tengu_session_file_read must fire on a successful text read");
        let md = &ev.metadata;
        // totalLines = 4 (trailing-newline phantom final line, TS readFileInRange).
        assert!(matches!(md.get("totalLines"), Some(AnalyticsValue::Int(4))));
        // A full read includes the terminal empty line: readLines = 4.
        assert!(matches!(md.get("readLines"), Some(AnalyticsValue::Int(4))));
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
            crate::generated_result(
                &json!({ "file_path": target.to_str().unwrap(), "offset": 2, "limit": 2 }),
            ),
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
        // 2 selected line bodies are joined as "l2\nl3".
        assert!(matches!(md.get("readLines"), Some(AnalyticsValue::Int(2))));
        // readBytes = 5; totalBytes = the full file's 12 bytes.
        assert!(matches!(md.get("readBytes"), Some(AnalyticsValue::Int(5))));
        assert!(matches!(
            md.get("totalBytes"),
            Some(AnalyticsValue::Int(12))
        ));
        // ext "txt" present.
        assert!(matches!(md.get("ext"), Some(AnalyticsValue::String(s)) if s == "txt"));
    }

    // Seeded context, partial views and changed mtimes all receive fresh reads.

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
    async fn seeded_context_ranged_read_returns_fresh_data() {
        // T3: `t===1 && r===void 0` — any other offset, or ANY limit, skips
        // the seeded branch and returns the full/ranged content.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("LINGXI.md");
        std::fs::write(&target, "a\nb\nc\n").unwrap();

        for input in [
            json!({ "file_path": target.to_str().unwrap(), "offset": 2 }),
            json!({ "file_path": target.to_str().unwrap(), "limit": 2 }),
        ] {
            let (ctx, _sink) = make_ctx(&tmp);
            let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
            seed_entry(&ctx, &route, "a\nb\nc\n", false);
            let tool = FileReadTool::new(ctx);
            let r = tool
                .call(crate::generated_result(&input), fresh_ctx(), fresh_tx())
                .await
                .unwrap();
            assert_ne!(
                crate::test_data(&r.data)["type"],
                "file_unchanged",
                "ranged read must not dedup: {input}"
            );
            assert!(
                crate::test_data(&r.data)["file"]["content"].is_string(),
                "{input}"
            );
        }
    }

    #[tokio::test]
    async fn seeded_partial_view_receives_full_read() {
        // T4: `!_.isPartialView` — a memory file whose recorded content differs
        // from disk (frontmatter/HTML comments stripped) must be reread; the
        // model never saw the on-disk bytes.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("cond.md");
        std::fs::write(&target, "---\npaths: src/**\n---\nbody\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        seed_entry(&ctx, &route, "---\npaths: src/**\n---\nbody\n", true);
        let tool = FileReadTool::new(ctx);

        let r = tool
            .call(
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_ne!(crate::test_data(&r.data)["type"], "file_unchanged");
        assert!(crate::test_data(&r.data)["file"]["content"].is_string());
    }

    #[tokio::test]
    async fn seeded_context_changed_mtime_returns_fresh_data() {
        // T5: `await b0e(f)===_.timestamp`.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("LINGXI.md");
        std::fs::write(&target, "v1\n").unwrap();
        let (ctx, _sink) = make_ctx(&tmp);
        let route = crate::normalize_model_file_path(target.to_str().unwrap(), &ctx.cwd());
        seed_entry(&ctx, &route, "v1\n", false);
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_ne!(crate::test_data(&r.data)["type"], "file_unchanged");
        assert_eq!(crate::test_data(&r.data)["file"]["content"], "v2\n");
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
            crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("image read must succeed");

        // Binary result shape: `{type:"image", file:{base64, type, originalSize}}`.
        // The dispatch loop derives the tool_result `content_blocks` from THIS
        // data (result-mapper `case "image"`), so the base64 + media type must be
        // present here — and the image must NOT also ride an injected message.
        assert_eq!(crate::test_data(&res.data)["type"], "image");
        assert_eq!(crate::test_data(&res.data)["file"]["type"], "image/png");
        assert!(crate::test_data(&res.data)["file"]["base64"]
            .as_str()
            .is_some_and(|b| !b.is_empty()));
        // An 8x8 PNG is not resized → no dimensions, no resize-note message.
        assert!(crate::test_data(&res.data)["file"]
            .get("dimensions")
            .is_none());
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let canon = std::fs::canonicalize(&target).unwrap();
        match err {
            ToolError::Io(m) => {
                let expected = crate::image_read::format_image_bad_magic(
                    &canon,
                    b"<!DOCTYPE html><html><head><title>Login</title>",
                );
                assert_eq!(m.string_at(""), Some(expected.as_str()));
            }
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let canon = std::fs::canonicalize(&target).unwrap();
        match err {
            ToolError::Io(m) => {
                let expected = format!("Image file is empty: {}", canon.display());
                assert_eq!(m.string_at(""), Some(expected.as_str()));
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("image read must succeed");

        // Resized → data.file.dimensions present (claude tbo's dimensions).
        assert_eq!(
            crate::test_data(&res.data)["file"]["dimensions"]["originalWidth"],
            4000
        );
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
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
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
                crate::generated_result(
                    &json!({ "file_path": target.to_str().unwrap(), "pages": "1-25" }),
                ),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            source_text(&err.model_facing_message()).contains("exceeds maximum"),
            "got: {err}"
        );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            source_text(&err.model_facing_message()).contains("not a valid PDF"),
            "got: {err}"
        );
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("small PDF read must succeed");

        // The tool-result data is text-only and tagged as a PDF.
        assert_eq!(crate::test_data(&res.data)["type"], "pdf");

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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                call_ctx,
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            source_text(&err.model_facing_message()).contains(
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("4MB supported-model PDF must inline, not error");
        assert_eq!(crate::test_data(&res.data)["type"], "pdf");
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
                crate::generated_result(&json!({ "file_path": target.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            source_text(&err.model_facing_message())
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
        let (sources, data, model_content) = build_pages_payload(
            std::path::Path::new("/docs/report.pdf"),
            4096,
            pages,
            &CancellationToken::new(),
        )
        .unwrap();

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
                crate::generated_result(&json!({ "file_path": missing.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = source_text(&err.model_facing_message());
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
                crate::generated_result(&json!({ "file_path": missing.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = source_text(&err.model_facing_message());

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
                crate::generated_result(&json!({ "file_path": missing.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = source_text(&err.model_facing_message());
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
                crate::generated_result(&json!({ "file_path": missing.to_str().unwrap() })),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let msg = source_text(&err.model_facing_message());
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
                crate::generated_result(&json!({ "file_path": requested.to_str().unwrap() })),
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
