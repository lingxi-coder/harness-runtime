//! MCP large-output processing (the model-facing size guard).
//!
//! 1:1 port of claude-code's `processMCPResult` and its `mcpValidation.ts`
//! helpers (`services/mcp/client.ts:2708-2799`, `utils/mcpValidation.ts`,
//! `utils/mcpOutputStorage.ts:16-59`). Run AFTER
//! [`crate::transform_result::transform_result_content`] has reshaped the raw
//! server blocks into model-facing content: this module decides whether the
//! result is too large for the context window and, if so, either
//!
//! * **persists** the full serialized content to a file and replaces the
//!   model-facing text with [`get_large_output_instructions`] (the default —
//!   non-image content, large-output-files feature on), or
//! * **truncates** it in place ([`truncate_mcp_content`]) — when the content
//!   carries images (persisting them as JSON would defeat image compression /
//!   viewability), or when the `ENABLE_MCP_LARGE_OUTPUT_FILES` env flag is set
//!   to a falsy value, or when the file write fails.
//!
//! The size decision mirrors Claude's two-stage guard (`AJr`): a cheap
//! 12 500-token estimate first, followed by an exact provider count of the FULL
//! content (image blocks included) against the 25 000-token cap. A count-call
//! FAILURE on a route that HAS a count endpoint (Anthropic) forwards the content
//! verbatim (`AJr`'s `catch`→`false`); only routes with NO count endpoint retain
//! the conservative persist/truncate fallback (the accepted multi-provider
//! divergence). The image branch reuses the shared image codec and compresses a
//! valid image to the remaining character budget — but is reached only AFTER the
//! exact count confirms the content exceeds the cap.
//!
//! Persistence REUSES the MCP-5d storage helper
//! [`mcp::persist_binary_content`] (writing the serialized UTF-8 string as bytes
//! with a `.json`/`.txt` extension, mirroring `persistToolResult`'s
//! `getToolResultPath(id, isJson)`); no new fs mechanism is introduced.

use std::path::Path;

use mcp::normalization::normalize_name_for_mcp;
use mcp::{decode_base64, persist_binary_content, PersistBinaryResult};
use serde_json::Value;

/// `DEFAULT_MAX_MCP_OUTPUT_TOKENS` (`mcpValidation.ts:16`): the MCP output token
/// cap. claude-code lets `MAX_MCP_OUTPUT_TOKENS` / a GrowthBook flag override
/// this; the port uses the hardcoded default (no growthbook layer).
pub const DEFAULT_MAX_MCP_OUTPUT_TOKENS: u64 = 25_000;

/// `IMAGE_TOKEN_ESTIMATE` (`mcpValidation.ts:15`): the per-image token estimate
/// used by [`content_size_estimate`] and the image char budget in
/// [`truncate_mcp_content`] (`IMAGE_TOKEN_ESTIMATE * 4` chars).
pub const IMAGE_TOKEN_ESTIMATE: u64 = 1_600;

/// `MCP_TOKEN_COUNT_THRESHOLD_FACTOR` (`mcpValidation.ts:14`) applied to
/// [`DEFAULT_MAX_MCP_OUTPUT_TOKENS`]: `25_000 * 0.5 = 12_500`. Content whose
/// rough token estimate exceeds this proceeds to exact provider counting.
pub const MCP_TRUNCATION_THRESHOLD_TOKENS: u64 = DEFAULT_MAX_MCP_OUTPUT_TOKENS / 2;

/// Process a transformed MCP tool-result `content` Value for the model.
///
/// 1:1 with `processMCPResult` (`client.ts:2720-2799`):
/// * IDE-server results bypass large-output handling (they never go to the
///   model directly) — returned unchanged.
/// * Content that does not exceed the threshold is returned unchanged.
/// * Over-threshold content is persisted to `output_dir` and replaced with
///   [`get_large_output_instructions`], UNLESS it contains images or the
///   `ENABLE_MCP_LARGE_OUTPUT_FILES` env flag is falsy or the write fails, in
///   which case it falls back to [`truncate_mcp_content`].
///
/// `now_millis` (the `persist_id_seed` wall clock) feeds the persisted-file id
/// `mcp-<server>-<tool>-<now_millis>`, mirroring the TS `Date.now()` template.
#[must_use]
pub fn process_mcp_result(
    content: &Value,
    server_name: &str,
    tool_name: &str,
    output_dir: &Path,
    now_millis: u128,
) -> Value {
    process_mcp_result_detailed(
        content,
        server_name,
        tool_name,
        output_dir,
        now_millis,
        ExactCountOutcome::Unsupported,
        inferred_result_type(content),
    )
    .content
}

/// Outcome of the exact provider token-count stage for the large-output guard.
///
/// Distinguishes the three branches of the oracle's needs-truncation predicate
/// `AJr` (`client.ts`, binary offset 229866928):
/// `async function AJr(e){if(!e)return!1;if(iHt(e)<=Fmo()*Ds_)return!1;try{let
/// n=await aHt([{role:"user",content:e}],[]);return!!(n&&n>Fmo())}catch(r){return
/// ke(r),!1}}` — the exact count (`aHt`) receives the FULL content (image blocks
/// included), and a count-call FAILURE (`catch`) returns `false`, i.e. the
/// content is forwarded to the model VERBATIM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExactCountOutcome {
    /// The active route returned an exact token count (`aHt` resolved to a
    /// number). Compared against the 25 000-token cap.
    Counted(u64),
    /// The route HAS an exact-count endpoint (Anthropic) but the count call
    /// FAILED. Mirrors `AJr`'s `catch`→`false`: forward the content verbatim
    /// rather than losing a valid MCP result to a transient count failure.
    CountFailed,
    /// The route has NO exact-count endpoint. This is the accepted
    /// multi-provider divergence (the oracle is always on an Anthropic route):
    /// stay conservative and apply large-output handling from the rough
    /// estimate alone.
    Unsupported,
}

/// The transformed MCP result shape before large-output handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpLargeResultType {
    ToolResult,
    StructuredContent,
    ContentArray,
}

impl McpLargeResultType {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::ToolResult => "toolResult",
            Self::StructuredContent => "structuredContent",
            Self::ContentArray => "contentArray",
        }
    }
}

/// Telemetry metadata for an MCP large-result handling branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LargeResultHandledTelemetry {
    pub outcome: &'static str,
    pub reason: &'static str,
    pub size_estimate_tokens: u64,
    pub persisted_size_chars: Option<u64>,
    pub result_type: Option<&'static str>,
    pub block_count: Option<u64>,
    pub persisted_as: Option<&'static str>,
}

/// Detailed MCP large-result processing outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessedMcpResult {
    pub content: Value,
    pub telemetry: Option<LargeResultHandledTelemetry>,
}

/// Process an MCP result with the provider's exact token count when available.
/// See [`ExactCountOutcome`] for the three route states this handles.
#[must_use]
pub fn process_mcp_result_with_exact_count(
    content: &Value,
    server_name: &str,
    tool_name: &str,
    output_dir: &Path,
    now_millis: u128,
    exact_token_count: ExactCountOutcome,
) -> Value {
    process_mcp_result_detailed(
        content,
        server_name,
        tool_name,
        output_dir,
        now_millis,
        exact_token_count,
        inferred_result_type(content),
    )
    .content
}

/// Process an MCP result with exact-count routing and return both the content
/// and any `tengu_mcp_large_result_handled` branch data.
#[must_use]
pub fn process_mcp_result_detailed(
    content: &Value,
    server_name: &str,
    tool_name: &str,
    output_dir: &Path,
    now_millis: u128,
    exact_token_count: ExactCountOutcome,
    result_type: McpLargeResultType,
) -> ProcessedMcpResult {
    let large_output_files_setting = std::env::var("ENABLE_MCP_LARGE_OUTPUT_FILES").ok();
    process_mcp_result_detailed_with_setting(
        content,
        server_name,
        tool_name,
        output_dir,
        now_millis,
        exact_token_count,
        result_type,
        large_output_files_setting.as_deref(),
    )
}

fn process_mcp_result_detailed_with_setting(
    content: &Value,
    server_name: &str,
    tool_name: &str,
    output_dir: &Path,
    now_millis: u128,
    exact_token_count: ExactCountOutcome,
    result_type: McpLargeResultType,
    large_output_files_setting: Option<&str>,
) -> ProcessedMcpResult {
    // IDE tools are not going to the model directly (client.ts:2727-2731).
    if server_name == "ide" {
        return ProcessedMcpResult {
            content: content.clone(),
            telemetry: None,
        };
    }

    // Under the large-output threshold → forward verbatim (client.ts:2733-2736).
    if !mcp_content_needs_exact_count(content) {
        return ProcessedMcpResult {
            content: content.clone(),
            telemetry: None,
        };
    }

    // The `AJr` needs-truncation predicate: Claude only enters large-output
    // handling when the exact count exceeds the full cap.
    match exact_token_count {
        // `AJr` `catch`→`false`: a count-endpoint failure on a route that HAS a
        // count endpoint (Anthropic) forwards the content verbatim.
        ExactCountOutcome::CountFailed => {
            return ProcessedMcpResult {
                content: content.clone(),
                telemetry: None,
            };
        }
        // `!!(n && n > Fmo())` is false when the exact count is within the cap.
        ExactCountOutcome::Counted(tokens) if tokens <= DEFAULT_MAX_MCP_OUTPUT_TOKENS => {
            return ProcessedMcpResult {
                content: content.clone(),
                telemetry: None,
            };
        }
        // Exact count over the cap, OR a route with no exact-count endpoint
        // (the accepted multi-provider divergence stays conservative): fall
        // through to large-output handling.
        ExactCountOutcome::Counted(_) | ExactCountOutcome::Unsupported => {}
    }
    let size_estimate_tokens = content_size_estimate(content);

    // Feature gate: an explicitly-falsy ENABLE_MCP_LARGE_OUTPUT_FILES reverts to
    // the old truncation behavior (client.ts:2741-2748). Unset → persist.
    if is_env_defined_falsy(large_output_files_setting) {
        return ProcessedMcpResult {
            content: truncate_mcp_content(content),
            telemetry: Some(LargeResultHandledTelemetry {
                outcome: "truncated",
                reason: "env_disabled",
                size_estimate_tokens,
                persisted_size_chars: None,
                result_type: None,
                block_count: None,
                persisted_as: None,
            }),
        };
    }

    // Images: persisting as JSON defeats image compression / viewability, so
    // fall back to truncation (client.ts:2756-2765).
    if content_contains_images(content) {
        return ProcessedMcpResult {
            content: truncate_mcp_content(content),
            telemetry: Some(LargeResultHandledTelemetry {
                outcome: "truncated",
                reason: "contains_images",
                size_estimate_tokens,
                persisted_size_chars: None,
                result_type: None,
                block_count: None,
                persisted_as: None,
            }),
        };
    }

    // Singleton-unwrap (`tengu_mcp_singleton_unwrap`, Statsig default true ⇒
    // always-on here): a transformed content array of exactly ONE `text` block
    // with no `annotations`/`_meta` is unwrapped to that block's raw text and
    // treated exactly like a bare string — persisted as plain text (.txt) with
    // line stats. Binary `processMCPResult`: `f = …d[0].text`, `h = typeof
    // d==="string"?d:f??Pe(d,null,2)`, `g = i==="toolResult" || f!==void 0`.
    let unwrapped: Option<String> = singleton_text_unwrap(content);
    let is_plain_text = unwrapped.is_some() || matches!(content, Value::String(_));

    // Serialize for persistence (client.ts:2771-2772): a bare string (or an
    // unwrapped singleton text block) is written as-is (.txt); other array
    // content is pretty-printed JSON (.json), matching `getToolResultPath`.
    let (content_str, mime): (String, &str) = match (&unwrapped, content) {
        (Some(text), _) => (text.clone(), "text/plain"),
        (None, Value::String(s)) => (s.clone(), "text/plain"),
        (None, other) => (
            serde_json::to_string_pretty(other).unwrap_or_default(),
            "application/json",
        ),
    };
    // `contentLength.toLocaleString()` operates on JavaScript string length
    // (UTF-16 code units), including for an astral character in plain text.
    let content_length = js_string_len(&content_str);

    let persist_id = format!(
        "mcp-{}-{}-{now_millis}",
        normalize_name_for_mcp(server_name),
        normalize_name_for_mcp(tool_name),
    );

    match persist_binary_content(content_str.as_bytes(), Some(mime), &persist_id, output_dir) {
        // File saved → hand the model the read-it-from-disk instructions
        // (client.ts:2786-2798). Line stats accompany the plain-text
        // (`toolResult`) shape only — `h = i==="toolResult" || f!==void 0`
        // (client.ts:2773); array/JSON content carries no `{count,maxLen}`.
        PersistBinaryResult::Ok { filepath, .. } => {
            // `g = i==="toolResult" || f!==void 0`: line stats accompany the
            // plain-text shape (bare string OR unwrapped singleton text block).
            let line_stats = if is_plain_text {
                Some(compute_line_stats(&content_str))
            } else {
                None
            };
            ProcessedMcpResult {
                content: Value::String(get_large_output_instructions(
                    &filepath,
                    content_length,
                    &format_description(content, is_plain_text),
                    line_stats.as_ref(),
                )),
                telemetry: Some(LargeResultHandledTelemetry {
                    outcome: "persisted",
                    reason: "file_saved",
                    size_estimate_tokens,
                    persisted_size_chars: Some(content_length),
                    result_type: Some(result_type.wire_str()),
                    block_count: content.as_array().map(|blocks| blocks.len() as u64),
                    persisted_as: Some(if is_plain_text { "text" } else { "json" }),
                }),
            }
        }
        // Write failed → the persist-failed truncation-info message
        // (client.ts:2775-2784).
        PersistBinaryResult::Err { error } => ProcessedMcpResult {
            content: Value::String(persist_failed_message(content_length, &error)),
            telemetry: Some(LargeResultHandledTelemetry {
                outcome: "truncated",
                reason: "persist_failed",
                size_estimate_tokens,
                persisted_size_chars: None,
                result_type: None,
                block_count: None,
                persisted_as: None,
            }),
        },
    }
}

/// Whether `content`'s rough token estimate exceeds the large-output threshold.
///
/// First stage of `mcpContentNeedsTruncation` (`mcpValidation.ts:151-178`):
/// `getContentSizeEstimate(content) > getMaxMcpOutputTokens() *
/// MCP_TOKEN_COUNT_THRESHOLD_FACTOR`. Callers then ask the active provider for
/// an exact count before applying the 25 000-token cap.
#[must_use]
pub fn mcp_content_needs_exact_count(content: &Value) -> bool {
    content_size_estimate(content) > MCP_TRUNCATION_THRESHOLD_TOKENS
}

/// Whether any block in `content` is an image block.
///
/// Port of `contentContainsImages` (`client.ts:2713-2718`): a string (or any
/// non-array) is never image-bearing; an array is scanned for a `type ==
/// "image"` block.
#[must_use]
pub fn content_contains_images(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(is_image_block))
}

fn inferred_result_type(content: &Value) -> McpLargeResultType {
    match content {
        Value::Array(_) => McpLargeResultType::ContentArray,
        Value::String(_) => McpLargeResultType::ToolResult,
        _ => McpLargeResultType::ContentArray,
    }
}

// -- internals ---------------------------------------------------------------

/// Rough token estimate for a content Value.
///
/// Port of `getContentSizeEstimate` (`mcpValidation.ts:59-75`): a string →
/// `roughTokenCountEstimation`; an array → sum of `roughTokenCountEstimation`
/// over text blocks plus [`IMAGE_TOKEN_ESTIMATE`] per image block (other blocks
/// contribute 0). Any other Value contributes 0 (not a valid `MCPToolResult`).
fn content_size_estimate(content: &Value) -> u64 {
    match content {
        Value::String(s) => rough_token_count_estimation(s),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => rough_token_count_estimation(
                    block.get("text").and_then(Value::as_str).unwrap_or(""),
                ),
                Some("image") => IMAGE_TOKEN_ESTIMATE,
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// `Math.round(content.length / 4)` (`tokenEstimation.ts:203-208`). Round-half-up
/// over a nonnegative byte length is `(len + 2) / 4`. Uses the UTF-8 byte length
/// (TS uses the UTF-16 string length); identical for ASCII, the same convention
/// the microcompact port uses.
fn rough_token_count_estimation(content: &str) -> u64 {
    js_string_len(content).saturating_add(2) / 4
}

/// JavaScript `String.prototype.length` for valid Rust strings.
fn js_string_len(content: &str) -> u64 {
    u64::try_from(content.encode_utf16().count()).unwrap_or(u64::MAX)
}

/// `block.type === 'image'`.
fn is_image_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("image")
}

/// `getFormatDescription(type, schema)` (`mcpOutputStorage.ts:16-28`) for the
/// two content shapes this path persists: the plain-text shape (`toolResult` —
/// a bare string OR an unwrapped singleton text block) is `"Plain text"`; other
/// array content is `contentArray` → `"JSON array with schema: <schema>"` where
/// `<schema>` comes from [`infer_compact_schema`].
fn format_description(content: &Value, is_plain_text: bool) -> String {
    if is_plain_text {
        "Plain text".to_string()
    } else {
        format!(
            "JSON array with schema: {}",
            infer_compact_schema(content, 2)
        )
    }
}

/// Singleton-unwrap (`tengu_mcp_singleton_unwrap`, Statsig default true): when a
/// transformed MCP content array is exactly ONE `text` block with no
/// `annotations`/`_meta`, the persisted form is that block's raw text (treated
/// as plain text), not the JSON array. Binary `processMCPResult`:
/// `f = p && Array.isArray(d) && d.length===1 && d[0]?.type==="text"
///      && !("annotations" in d[0]) && !("_meta" in d[0]) ? d[0].text : void 0`.
fn singleton_text_unwrap(content: &Value) -> Option<String> {
    let arr = content.as_array()?;
    if arr.len() != 1 {
        return None;
    }
    let obj = arr[0].as_object()?;
    if obj.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    if obj.contains_key("annotations") || obj.contains_key("_meta") {
        return None;
    }
    obj.get("text").and_then(Value::as_str).map(String::from)
}

/// `inferCompactSchema(value, depth)` (`client.ts:2644-2660`): a compact,
/// jq-friendly type signature, e.g. `{title: string, items: [{id: number}]}`.
fn infer_compact_schema(value: &Value, depth: i32) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Array(items) => {
            if items.is_empty() {
                "[]".to_string()
            } else {
                format!("[{}]", infer_compact_schema(&items[0], depth - 1))
            }
        }
        Value::Object(map) => {
            if depth <= 0 {
                "{...}".to_string()
            } else {
                let props: Vec<String> = map
                    .iter()
                    .take(10)
                    .map(|(k, v)| format!("{k}: {}", infer_compact_schema(v, depth - 1)))
                    .collect();
                let suffix = if map.len() > 10 { ", ..." } else { "" };
                format!("{{{}{suffix}}}", props.join(", "))
            }
        }
        // JS `typeof`: string / number / boolean.
        Value::String(_) => "string".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::Bool(_) => "boolean".to_string(),
    }
}

/// Per-line statistics for a persisted plain-text result: `{count, maxLen}`
/// (`client.ts:2773-2778`). `count` is the line count after dropping one
/// trailing empty line; `max_len` is the longest line. TS measures `String`
/// `.length` (UTF-16 units).
struct LineStats {
    count: u64,
    max_len: u64,
}

/// `A.split("\n")` → drop one trailing empty element → `{count, maxLen}`
/// (`client.ts:2774-2778`).
fn compute_line_stats(content: &str) -> LineStats {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    let max_len = lines
        .iter()
        .map(|line| js_string_len(line))
        .max()
        .unwrap_or(0);
    LineStats {
        count: lines.len() as u64,
        max_len,
    }
}

/// `R$d` (`mcpOutputStorage`): the default file-read max-output token budget.
const DEFAULT_FILE_READ_MAX_OUTPUT_TOKENS: u64 = 25_000;

/// `AOg` (`mcpOutputStorage`): `LINGXI_FILE_READ_MAX_OUTPUT_TOKENS` when it
/// parses to a valid positive integer, else [`DEFAULT_FILE_READ_MAX_OUTPUT_TOKENS`].
/// The value is parsed by claude-code's shared `hp` helper
/// ([`lingxi_core::host::env::parse_int_env`]) — `if(e){let t=hp(e);if(!isNaN(t)&&t>0)return t}` —
/// which since 2.1.211 accepts scientific notation and digit-group separators.
/// The `tengu_amber_wren` Statsig config layer is unportable and omitted; its
/// `{}` default selects the same constant, so this matches the runtime default.
fn file_read_max_output_tokens() -> u64 {
    if let Ok(raw) = std::env::var("LINGXI_FILE_READ_MAX_OUTPUT_TOKENS") {
        let t = lingxi_core::host::env::parse_int_env(&raw);
        if !t.is_nan() && t > 0.0 {
            return t as u64;
        }
    }
    DEFAULT_FILE_READ_MAX_OUTPUT_TOKENS
}

/// `getLargeOutputInstructions` (= `F7r`, `mcpOutputStorage`) for the default
/// (`!lHn()`) prompt path: the `tengu_mcp_subagent_prompt` gate is off by
/// default, so the format-aware jq/python/grep branches are not emitted. The
/// MCP caller passes `maxReadLength` undefined and `line_stats` only for the
/// plain-text shape. `content_length`/line counts render with en-US comma
/// grouping (`Number.prototype.toLocaleString`).
fn get_large_output_instructions(
    raw_output_path: &str,
    content_length: u64,
    format_description: &str,
    line_stats: Option<&LineStats>,
) -> String {
    // `Error: result (${o!==void 0 ? "${t} characters across ${o.count} line(s)"
    // : "${t} characters"})` (client.ts).
    let count_phrase = match line_stats {
        Some(ls) => format!(
            "{} characters across {} {}",
            to_locale_string(content_length),
            to_locale_string(ls.count),
            if ls.count == 1 { "line" } else { "lines" },
        ),
        None => format!("{} characters", to_locale_string(content_length)),
    };
    // a = Math.floor(maxTokens * 4 * 0.8) == maxTokens * 16 / 5 (exact floor over
    // the ×4 char budget). `c` holds when there are multiple lines all within the
    // budget; `lines_too_long` (the note) shows for a single line or an
    // over-budget line.
    let a = file_read_max_output_tokens().saturating_mul(16) / 5;
    let c = line_stats.is_some_and(|ls| ls.count > 1 && ls.max_len <= a);
    let lines_too_long = line_stats.is_some() && !c;
    format!(
        "Error: result ({count_phrase}) exceeds maximum allowed tokens. Output has been saved to {raw_output_path}.\n\
         Format: {format_description}\n\
         Use offset and limit parameters to read specific portions of the file, search within it for specific content, and jq to make structured queries.\n\
         REQUIREMENTS FOR SUMMARIZATION/ANALYSIS/REVIEW:\n\
         {requirements}",
        requirements = summarization_requirements(raw_output_path, None, lines_too_long),
    )
}

/// `$$d(rawOutputPath, maxReadLength, linesTooLong)` (`mcpOutputStorage`): the
/// REQUIREMENTS bullet list. `max_read_length` is `Some` only on the Bash/file
/// path (its char budget appears in the truncation-warning bullet); the MCP
/// path passes `None`. `lines_too_long` inserts the shell-slice note before the
/// truncation-warning bullet.
fn summarization_requirements(
    raw_output_path: &str,
    max_read_length: Option<u64>,
    lines_too_long: bool,
) -> String {
    let truncation_bullet = match max_read_length {
        Some(n) => format!(
            "- If you receive truncation warnings when reading the file (\"[N lines truncated]\"), reduce the chunk size until you have read 100% of the content without truncation ***DO NOT PROCEED UNTIL YOU HAVE DONE THIS***. Bash output is limited to {} chars.\n",
            to_locale_string(n),
        ),
        None =>
            "- If you receive truncation warnings when reading the file, reduce the chunk size until you have read 100% of the content without truncation.\n".to_string(),
    };
    let lines_note = if lines_too_long {
        "- Note: this file's lines are too long for Read's offset/limit chunking. If a shell tool is available, slice by character range (e.g. python read()[A:B], dd, or cut -c) instead.\n"
    } else {
        ""
    };
    format!(
        "- You MUST read the content from the file at {raw_output_path} in sequential chunks until 100% of the content has been read.\n\
         {lines_note}{truncation_bullet}\
         - Before producing ANY summary or analysis, you MUST explicitly describe what portion of the content you have read. ***If you did not read the entire content, you MUST explicitly state this.***\n\
         - If after a few attempts you cannot read the file (file not found, lines too long for Read's offset/limit, no shell access), STOP retrying. Summarize what you were able to read, explicitly state which portion you could not read and why, and proceed.\n"
    )
}

/// The persist-failed fallback (`client.ts:2783`): truncation-info text naming
/// the byte size and the write error.
fn persist_failed_message(content_length: u64, error: &str) -> String {
    format!(
        "Error: result ({} characters) exceeds maximum allowed tokens. Failed to save output to file: {error}. If this MCP server provides pagination or filtering tools, use them to retrieve specific portions of the data.",
        to_locale_string(content_length)
    )
}

/// `truncateMcpContent` (`mcpValidation.ts:180-198`): truncate to
/// `getMaxMcpOutputChars()` (= `maxTokens * 4`) and append [`truncation_message`].
/// A string is sliced + suffixed; an array is run through
/// [`truncate_content_blocks`] with the message appended as a final text block.
fn truncate_mcp_content(content: &Value) -> Value {
    let max_chars = usize::try_from(DEFAULT_MAX_MCP_OUTPUT_TOKENS * 4).unwrap_or(usize::MAX);
    let message = truncation_message();
    match content {
        Value::String(s) => Value::String(format!("{}{message}", truncate_string(s, max_chars))),
        Value::Array(blocks) => {
            let mut out = truncate_content_blocks(blocks, max_chars);
            out.push(text_block(&message));
            Value::Array(out)
        }
        other => other.clone(),
    }
}

/// `getTruncationMessage()` (`mcpValidation.ts:81-85`). Byte-locked.
fn truncation_message() -> String {
    format!(
        "\n\n[OUTPUT TRUNCATED - exceeded {DEFAULT_MAX_MCP_OUTPUT_TOKENS} token limit]\n\nThe tool output was truncated. If this MCP server provides pagination or filtering tools, use them to retrieve specific portions of the data. If pagination is not available, inform the user that you are working with truncated output and results may be incomplete."
    )
}

/// Oracle `rs`: the first `max_chars` JavaScript iterator elements. JS string
/// iteration is Unicode-scalar-safe, matching Rust's `chars()` here.
fn truncate_string(content: &str, max_chars: usize) -> String {
    match content.char_indices().nth(max_chars) {
        Some((idx, _)) => content[..idx].to_string(),
        None => content.to_string(),
    }
}

/// Oracle `ce`: take a prefix by UTF-16 code-unit budget without retaining a
/// dangling high surrogate. Rust strings cannot hold an isolated surrogate,
/// so stopping before the scalar that would cross the budget is equivalent.
fn truncate_string_utf16(content: &str, max_code_units: usize) -> String {
    let mut used = 0usize;
    let mut end = 0usize;
    for (index, ch) in content.char_indices() {
        let units = ch.len_utf16();
        if used.saturating_add(units) > max_code_units {
            break;
        }
        used += units;
        end = index + ch.len_utf8();
    }
    if end == content.len() {
        content.to_string()
    } else {
        content[..end].to_string()
    }
}

/// `truncateContentBlocks` (`mcpValidation.ts:94-149`): pack blocks into a
/// `max_chars` budget — text blocks are kept whole or sliced to fit; image
/// blocks cost `IMAGE_TOKEN_ESTIMATE * 4` chars and are compressed to the
/// remaining budget when that estimate no longer fits; any other block passes
/// through.
fn truncate_content_blocks(blocks: &[Value], max_chars: usize) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::new();
    let mut current_chars: usize = 0;
    let image_chars = usize::try_from(IMAGE_TOKEN_ESTIMATE).unwrap_or(usize::MAX) * 4;

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let remaining = max_chars.saturating_sub(current_chars);
                if remaining == 0 {
                    break;
                }
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                let text_len = usize::try_from(js_string_len(text)).unwrap_or(usize::MAX);
                if text_len <= remaining {
                    result.push(block.clone());
                    current_chars += text_len;
                } else {
                    result.push(text_block(&truncate_string_utf16(text, remaining)));
                    break;
                }
            }
            Some("image") => {
                if current_chars + image_chars <= max_chars {
                    result.push(block.clone());
                    current_chars += image_chars;
                } else {
                    let remaining = max_chars.saturating_sub(current_chars);
                    if let Some((compressed, chars)) =
                        compress_image_block_to_budget(block, remaining)
                    {
                        result.push(compressed);
                        current_chars = current_chars.saturating_add(chars);
                    }
                }
            }
            _ => result.push(block.clone()),
        }
    }
    result
}

fn compress_image_block_to_budget(block: &Value, max_chars: usize) -> Option<(Value, usize)> {
    let source = block.get("source")?.as_object()?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let data = source.get("data")?.as_str()?;
    let bytes = decode_base64(data).ok()?;
    let processed =
        tool_api::util::image_budget::process_image_with_base64_budget(bytes, max_chars).ok()?;
    if processed.base64.len() > max_chars {
        return None;
    }
    let chars = processed.base64.len();
    let mut compressed = block.clone();
    let compressed_source = compressed.get_mut("source")?.as_object_mut()?;
    compressed_source.insert("data".to_string(), Value::String(processed.base64));
    compressed_source.insert(
        "media_type".to_string(),
        Value::String(processed.media_type),
    );
    Some((compressed, chars))
}

/// Build a `{"type":"text","text":<text>}` content block.
fn text_block(text: &str) -> Value {
    serde_json::json!({ "type": "text", "text": text })
}

/// `Number.prototype.toLocaleString()` for a nonnegative integer under the
/// en-US locale: comma thousands separators (`1234567 -> "1,234,567"`).
fn to_locale_string(n: u64) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + (len.saturating_sub(1)) / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// `isEnvDefinedFalsy` (`utils/envUtils.ts:39-47`): a defined, non-empty value
/// normalizing (lowercase + trim) to `0`/`false`/`no`/`off`. Undefined or empty
/// is NOT falsy.
fn is_env_defined_falsy(env_var: Option<&str>) -> bool {
    match env_var {
        None | Some("") => false,
        Some(v) => matches!(v.to_lowercase().trim(), "0" | "false" | "no" | "off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A text block whose UTF-8 length yields `roughTokenCountEstimation`
    /// `(len + 2) / 4` tokens for the given char count.
    fn text_of_len(len: usize) -> Value {
        json!({ "type": "text", "text": "a".repeat(len) })
    }

    #[test]
    fn rough_estimate_matches_round_half_up() {
        assert_eq!(rough_token_count_estimation(""), 0);
        assert_eq!(rough_token_count_estimation("ab"), 1); // (2+2)/4 = 1
        assert_eq!(rough_token_count_estimation(&"a".repeat(50_000)), 12_500);
        assert_eq!(rough_token_count_estimation(&"a".repeat(50_004)), 12_501);
        // JS measures UTF-16 code units: two astral scalars are length four,
        // not Rust's two scalars or their eight UTF-8 bytes.
        assert_eq!(rough_token_count_estimation("😀😀"), 1);
    }

    #[test]
    fn needs_truncation_threshold_boundary() {
        // estimate == 12_500 (== threshold) → NOT over; 12_501 → over.
        let at = Value::Array(vec![text_of_len(50_000)]);
        let over = Value::Array(vec![text_of_len(50_004)]);
        assert_eq!(content_size_estimate(&at), 12_500);
        assert_eq!(content_size_estimate(&over), 12_501);
        assert!(
            !mcp_content_needs_exact_count(&at),
            "12500 tokens is at the boundary, not over"
        );
        assert!(
            mcp_content_needs_exact_count(&over),
            "12501 tokens exceeds the threshold"
        );
    }

    #[test]
    fn content_contains_images_detects_image_blocks() {
        let with_img = json!([
            { "type": "text", "text": "hi" },
            { "type": "image", "source": { "type": "base64", "data": "x", "media_type": "image/png" } },
        ]);
        let text_only = json!([{ "type": "text", "text": "hi" }]);
        assert!(content_contains_images(&with_img));
        assert!(!content_contains_images(&text_only));
        // A bare string is never image-bearing.
        assert!(!content_contains_images(&Value::String("hello".into())));
    }

    #[test]
    fn under_limit_returns_content_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "text", "text": "small result" }]);
        let out = process_mcp_result(&content, "srv", "tool", dir.path(), 1700);
        assert_eq!(
            out, content,
            "under-threshold content is forwarded verbatim"
        );
        // Nothing persisted.
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn exact_count_below_full_cap_cancels_large_output_handling() {
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(60_000)]);
        assert!(mcp_content_needs_exact_count(&content));

        let out = process_mcp_result_with_exact_count(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::Counted(DEFAULT_MAX_MCP_OUTPUT_TOKENS),
        );

        assert_eq!(out, content);
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn exact_count_above_full_cap_applies_large_output_handling() {
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(60_000)]);

        let out = process_mcp_result_with_exact_count(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::Counted(DEFAULT_MAX_MCP_OUTPUT_TOKENS + 1),
        );

        assert!(out.as_str().is_some());
        assert!(dir.path().join("mcp-srv-tool-1700.txt").exists());
    }

    #[test]
    fn count_failed_forwards_content_verbatim() {
        // Oracle `AJr` `catch`→`false`: when the route HAS a count endpoint
        // (Anthropic) but the count call fails, the over-rough-threshold content
        // is forwarded VERBATIM — it is NOT persisted or truncated.
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(60_000)]);
        assert!(mcp_content_needs_exact_count(&content));

        let out = process_mcp_result_with_exact_count(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::CountFailed,
        );

        assert_eq!(out, content, "count failure forwards content verbatim");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "nothing persisted on a count failure"
        );
    }

    #[test]
    fn count_failed_forwards_image_content_verbatim() {
        // A count failure forwards image-bearing content verbatim too — the
        // image-truncate branch is only reached AFTER an exact count exceeds the
        // cap, never on a count failure.
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![
            text_of_len(60_000),
            json!({ "type": "image", "source": { "type": "base64", "data": "x", "media_type": "image/png" } }),
        ]);

        let out = process_mcp_result_with_exact_count(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::CountFailed,
        );

        assert_eq!(out, content, "image content forwarded verbatim on failure");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn unsupported_route_stays_conservative() {
        // No exact-count endpoint (multi-provider divergence): over-rough-
        // threshold content still enters large-output handling from the rough
        // estimate — pure text persists to disk.
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(60_000)]);

        let out = process_mcp_result_with_exact_count(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::Unsupported,
        );

        assert!(
            out.as_str().is_some(),
            "unsupported route persists/truncates"
        );
        assert!(dir.path().join("mcp-srv-tool-1700.txt").exists());
    }

    #[test]
    fn counted_image_content_within_cap_forwards_verbatim() {
        // The `!content_contains_images` guard is gone: image-bearing content
        // whose exact count is within the cap is forwarded VERBATIM (image
        // preserved), not truncated. This is the primary parity fix — the old
        // code skipped counting for images and always truncated.
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![
            text_of_len(60_000),
            json!({ "type": "image", "source": { "type": "base64", "data": "x", "media_type": "image/png" } }),
        ]);
        assert!(mcp_content_needs_exact_count(&content));

        let out = process_mcp_result_with_exact_count(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::Counted(DEFAULT_MAX_MCP_OUTPUT_TOKENS),
        );

        assert_eq!(out, content, "image content within cap forwarded whole");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn counted_image_content_over_cap_truncates() {
        // Once the exact count exceeds the cap, image-bearing content DOES take
        // the truncate branch (persisting images as JSON defeats compression).
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![
            text_of_len(60_000),
            json!({ "type": "image", "source": { "type": "base64", "data": "x", "media_type": "image/png" } }),
        ]);

        let out = process_mcp_result_with_exact_count(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::Counted(DEFAULT_MAX_MCP_OUTPUT_TOKENS + 1),
        );

        // Truncation keeps the array shape and appends the truncation message.
        let blocks = out.as_array().expect("truncate path returns an array");
        assert_eq!(
            blocks.last().unwrap().get("text").and_then(Value::as_str),
            Some(truncation_message().as_str())
        );
        assert!(blocks.iter().any(is_image_block), "image kept");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn over_limit_no_images_persists_and_returns_instructions() {
        let dir = tempfile::tempdir().unwrap();
        // ~15000-token estimate (60000 chars / 4) → over the 12500 threshold.
        // TWO text blocks → NOT a singleton, so the JSON-array path (not the
        // singleton-unwrap plain-text path) is exercised here.
        let content = Value::Array(vec![text_of_len(30_000), text_of_len(30_000)]);
        let out = process_mcp_result(&content, "srv", "tool", dir.path(), 1700);

        let text = out.as_str().expect("persist path returns a string");
        // The file was written with the mcp-<server>-<tool>-<now>.json id.
        let expected_path = dir.path().join("mcp-srv-tool-1700.json");
        assert!(expected_path.exists(), "content persisted to disk");
        // On-disk bytes are the pretty-printed JSON of the content array.
        let on_disk = std::fs::read_to_string(&expected_path).unwrap();
        assert_eq!(on_disk, serde_json::to_string_pretty(&content).unwrap());

        // Byte-faithful instruction wording.
        assert!(text.starts_with("Error: result ("), "got: {text}");
        assert!(text.contains(&format!(
            "exceeds maximum allowed tokens. Output has been saved to {}.\n",
            expected_path.to_string_lossy()
        )));
        assert!(text.contains("Format: JSON array with schema: [{type: string, text: string}]\n"));
        assert!(text.contains("REQUIREMENTS FOR SUMMARIZATION/ANALYSIS/REVIEW:\n"));
        assert!(text.contains(
            "- If you receive truncation warnings when reading the file, reduce the chunk size until you have read 100% of the content without truncation.\n"
        ));
        assert!(text.contains(
            "***If you did not read the entire content, you MUST explicitly state this.***\n"
        ));
        // Array shape → no line stats → no "lines too long" note, bare count.
        assert!(text.starts_with("Error: result ("));
        assert!(
            !text.contains("characters across"),
            "array shape has no line count"
        );
        assert!(!text.contains("- Note: this file's lines are too long"));
        // v2.1.185 final bullet.
        assert!(text.ends_with("- If after a few attempts you cannot read the file (file not found, lines too long for Read's offset/limit, no shell access), STOP retrying. Summarize what you were able to read, explicitly state which portion you could not read and why, and proceed.\n"));
    }

    #[test]
    fn over_limit_singleton_text_block_unwraps_to_plaintext() {
        // Binary singleton-unwrap (tengu_mcp_singleton_unwrap): an over-limit
        // content array of exactly ONE text block (no annotations/_meta) is
        // persisted as PLAIN TEXT (.txt) with line stats + Format "Plain text".
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(60_000)]);
        let out = process_mcp_result(&content, "srv", "tool", dir.path(), 1700);

        let text = out.as_str().expect("persist path returns a string");
        // Persisted as .txt (plain text), and the on-disk bytes are the raw
        // unwrapped text — NOT the JSON array.
        let txt_path = dir.path().join("mcp-srv-tool-1700.txt");
        assert!(txt_path.exists(), "unwrapped singleton persisted as .txt");
        assert!(!dir.path().join("mcp-srv-tool-1700.json").exists());
        assert_eq!(
            std::fs::read_to_string(&txt_path).unwrap(),
            "a".repeat(60_000)
        );
        // Plain-text Format + line-count phrase (NOT the JSON-array schema).
        assert!(text.contains("Format: Plain text\n"), "got: {text}");
        assert!(!text.contains("JSON array with schema"));
        assert!(
            text.contains("characters across"),
            "plain text carries line stats"
        );
    }

    #[test]
    fn singleton_with_annotations_does_not_unwrap() {
        // `!("annotations" in d[0])`: a singleton text block carrying annotations
        // is NOT unwrapped — it stays the JSON-array path.
        let dir = tempfile::tempdir().unwrap();
        let content = json!([{ "type": "text", "text": "a".repeat(60_000), "annotations": {} }]);
        let out = process_mcp_result(&content, "srv", "tool", dir.path(), 1700);
        let text = out.as_str().expect("persist path returns a string");
        assert!(
            dir.path().join("mcp-srv-tool-1700.json").exists(),
            "stays JSON"
        );
        assert!(text.contains("JSON array with schema"), "got: {text}");
    }

    #[test]
    fn over_limit_with_images_truncates_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![
            text_of_len(60_000),
            json!({ "type": "image", "source": { "type": "base64", "data": "x", "media_type": "image/png" } }),
        ]);
        let out = process_mcp_result(&content, "srv", "tool", dir.path(), 1700);

        // Truncation keeps the array shape (NOT a Value::String instruction).
        let blocks = out.as_array().expect("truncate path returns an array");
        // Last block is the byte-locked truncation message.
        let last = blocks.last().unwrap();
        assert_eq!(last.get("type").and_then(Value::as_str), Some("text"));
        assert_eq!(
            last.get("text").and_then(Value::as_str).unwrap(),
            truncation_message()
        );
        // Image preserved (fits the 100k char budget after the text).
        assert!(
            blocks.iter().any(is_image_block),
            "image kept through truncation"
        );
        // Nothing was persisted to disk.
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn over_budget_image_is_compressed_into_remaining_character_budget() {
        const WIDE_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAC7gAAAABCAIAAADBtXRpAAAAH0lEQVR42u3BAQEAAACCIP+vbkhAAQAAAAAAAADAgQEjKQABp2QvZgAAAABJRU5ErkJggg==";
        let content = Value::Array(vec![
            text_of_len(97_000),
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "data": WIDE_PNG,
                    "media_type": "image/png"
                }
            }),
        ]);

        let out = truncate_mcp_content(&content);
        let image = out
            .as_array()
            .unwrap()
            .iter()
            .find(|block| is_image_block(block))
            .expect("compressed image remains present");
        let data = image["source"]["data"].as_str().unwrap();
        assert!(data.len() <= 3_000);
        assert_ne!(data, WIDE_PNG);
        assert_eq!(image["source"]["media_type"], json!("image/jpeg"));
    }

    #[test]
    fn ide_server_bypasses_processing() {
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(60_000)]);
        let out = process_mcp_result(&content, "ide", "tool", dir.path(), 1700);
        assert_eq!(out, content, "ide results bypass large-output handling");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn large_output_instructions_byte_layout_no_line_stats() {
        // Array/JSON shape (no line stats): bare "characters" count, no
        // "lines too long" note, plus the v2.1.185 final "STOP retrying" bullet.
        let s = get_large_output_instructions(
            "/tmp/out/mcp-srv-tool-1.json",
            1_234_567,
            "JSON array with schema: {...}",
            None,
        );
        let expected = "Error: result (1,234,567 characters) exceeds maximum allowed tokens. Output has been saved to /tmp/out/mcp-srv-tool-1.json.\n\
Format: JSON array with schema: {...}\n\
Use offset and limit parameters to read specific portions of the file, search within it for specific content, and jq to make structured queries.\n\
REQUIREMENTS FOR SUMMARIZATION/ANALYSIS/REVIEW:\n\
- You MUST read the content from the file at /tmp/out/mcp-srv-tool-1.json in sequential chunks until 100% of the content has been read.\n\
- If you receive truncation warnings when reading the file, reduce the chunk size until you have read 100% of the content without truncation.\n\
- Before producing ANY summary or analysis, you MUST explicitly describe what portion of the content you have read. ***If you did not read the entire content, you MUST explicitly state this.***\n\
- If after a few attempts you cannot read the file (file not found, lines too long for Read's offset/limit, no shell access), STOP retrying. Summarize what you were able to read, explicitly state which portion you could not read and why, and proceed.\n";
        assert_eq!(s, expected);
    }

    #[test]
    fn large_output_instructions_byte_layout_multiline_no_note() {
        // Plain-text, multiple lines all within budget (maxLen <= 80000): the
        // "across N lines" count form, but NO "lines too long" note.
        let ls = LineStats {
            count: 5,
            max_len: 40,
        };
        let s = get_large_output_instructions(
            "/tmp/out/mcp-srv-tool-1.txt",
            1_234_567,
            "Plain text",
            Some(&ls),
        );
        let expected = "Error: result (1,234,567 characters across 5 lines) exceeds maximum allowed tokens. Output has been saved to /tmp/out/mcp-srv-tool-1.txt.\n\
Format: Plain text\n\
Use offset and limit parameters to read specific portions of the file, search within it for specific content, and jq to make structured queries.\n\
REQUIREMENTS FOR SUMMARIZATION/ANALYSIS/REVIEW:\n\
- You MUST read the content from the file at /tmp/out/mcp-srv-tool-1.txt in sequential chunks until 100% of the content has been read.\n\
- If you receive truncation warnings when reading the file, reduce the chunk size until you have read 100% of the content without truncation.\n\
- Before producing ANY summary or analysis, you MUST explicitly describe what portion of the content you have read. ***If you did not read the entire content, you MUST explicitly state this.***\n\
- If after a few attempts you cannot read the file (file not found, lines too long for Read's offset/limit, no shell access), STOP retrying. Summarize what you were able to read, explicitly state which portion you could not read and why, and proceed.\n";
        assert_eq!(s, expected);
    }

    #[test]
    fn large_output_instructions_byte_layout_single_line_gets_note() {
        // Plain-text, a single line (count == 1 → c false → lines_too_long): the
        // "across 1 line" singular form AND the shell-slice note.
        let ls = LineStats {
            count: 1,
            max_len: 1_234_567,
        };
        let s = get_large_output_instructions(
            "/tmp/out/mcp-srv-tool-1.txt",
            1_234_567,
            "Plain text",
            Some(&ls),
        );
        let expected = "Error: result (1,234,567 characters across 1 line) exceeds maximum allowed tokens. Output has been saved to /tmp/out/mcp-srv-tool-1.txt.\n\
Format: Plain text\n\
Use offset and limit parameters to read specific portions of the file, search within it for specific content, and jq to make structured queries.\n\
REQUIREMENTS FOR SUMMARIZATION/ANALYSIS/REVIEW:\n\
- You MUST read the content from the file at /tmp/out/mcp-srv-tool-1.txt in sequential chunks until 100% of the content has been read.\n\
- Note: this file's lines are too long for Read's offset/limit chunking. If a shell tool is available, slice by character range (e.g. python read()[A:B], dd, or cut -c) instead.\n\
- If you receive truncation warnings when reading the file, reduce the chunk size until you have read 100% of the content without truncation.\n\
- Before producing ANY summary or analysis, you MUST explicitly describe what portion of the content you have read. ***If you did not read the entire content, you MUST explicitly state this.***\n\
- If after a few attempts you cannot read the file (file not found, lines too long for Read's offset/limit, no shell access), STOP retrying. Summarize what you were able to read, explicitly state which portion you could not read and why, and proceed.\n";
        assert_eq!(s, expected);
    }

    #[test]
    fn compute_line_stats_matches_ts_split() {
        // No trailing newline → all lines counted.
        let ls = compute_line_stats("a\nbb\nccc");
        assert_eq!((ls.count, ls.max_len), (3, 3));
        // A single trailing newline is dropped (TS `S.at(-1)===""` pop).
        let ls = compute_line_stats("a\nbb\nccc\n");
        assert_eq!((ls.count, ls.max_len), (3, 3));
        // A single line (no newline) → count 1.
        let ls = compute_line_stats("abcd");
        assert_eq!((ls.count, ls.max_len), (1, 4));
        // Empty string → `"".split("\n")` is `[""]` → count 1, maxLen 0.
        let ls = compute_line_stats("");
        assert_eq!((ls.count, ls.max_len), (1, 0));
        // Two trailing newlines: only ONE empty element is popped.
        let ls = compute_line_stats("x\n\n");
        assert_eq!((ls.count, ls.max_len), (2, 1));
        let ls = compute_line_stats("😀x\n旗");
        assert_eq!((ls.count, ls.max_len), (2, 3));
    }

    #[test]
    fn array_text_truncation_uses_surrogate_safe_utf16_budget() {
        assert_eq!(truncate_string_utf16("a😀b", 1), "a");
        assert_eq!(truncate_string_utf16("a😀b", 2), "a");
        assert_eq!(truncate_string_utf16("a😀b", 3), "a😀");
        assert_eq!(truncate_string_utf16("a😀b", 4), "a😀b");
    }

    #[test]
    fn truncation_message_byte_layout() {
        assert_eq!(
            truncation_message(),
            "\n\n[OUTPUT TRUNCATED - exceeded 25000 token limit]\n\nThe tool output was truncated. If this MCP server provides pagination or filtering tools, use them to retrieve specific portions of the data. If pagination is not available, inform the user that you are working with truncated output and results may be incomplete."
        );
    }

    #[test]
    fn to_locale_string_groups_thousands() {
        assert_eq!(to_locale_string(0), "0");
        assert_eq!(to_locale_string(999), "999");
        assert_eq!(to_locale_string(1_000), "1,000");
        assert_eq!(to_locale_string(1_234_567), "1,234,567");
    }

    #[test]
    fn string_content_over_limit_persists_as_txt() {
        let dir = tempfile::tempdir().unwrap();
        let content = Value::String("a".repeat(60_000));
        let out = process_mcp_result(&content, "srv", "tool", dir.path(), 1700);
        let text = out.as_str().unwrap();
        let expected_path = dir.path().join("mcp-srv-tool-1700.txt");
        assert!(
            expected_path.exists(),
            "string content persisted with .txt ext"
        );
        assert!(text.contains("Format: Plain text\n"));
    }

    #[test]
    fn detailed_env_disabled_branch_reports_truncation_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(60_000)]);
        let processed = process_mcp_result_detailed_with_setting(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::Unsupported,
            McpLargeResultType::ContentArray,
            Some("0"),
        );

        assert!(
            processed.content.is_array(),
            "env-disabled branch truncates"
        );
        assert_eq!(
            processed.telemetry,
            Some(LargeResultHandledTelemetry {
                outcome: "truncated",
                reason: "env_disabled",
                size_estimate_tokens: content_size_estimate(&content),
                persisted_size_chars: None,
                result_type: None,
                block_count: None,
                persisted_as: None,
            })
        );
    }

    #[test]
    fn detailed_persist_success_reports_oracle_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let content = Value::Array(vec![text_of_len(30_000), text_of_len(30_000)]);
        let processed = process_mcp_result_detailed(
            &content,
            "srv",
            "tool",
            dir.path(),
            1700,
            ExactCountOutcome::Unsupported,
            McpLargeResultType::ContentArray,
        );

        assert!(processed.content.as_str().is_some());
        assert_eq!(
            processed.telemetry,
            Some(LargeResultHandledTelemetry {
                outcome: "persisted",
                reason: "file_saved",
                size_estimate_tokens: content_size_estimate(&content),
                persisted_size_chars: Some(
                    serde_json::to_string_pretty(&content)
                        .unwrap()
                        .encode_utf16()
                        .count() as u64
                ),
                result_type: Some("contentArray"),
                block_count: Some(2),
                persisted_as: Some("json"),
            })
        );
    }

    #[test]
    fn env_falsy_helper_matches_ts() {
        assert!(is_env_defined_falsy(Some("0")));
        assert!(is_env_defined_falsy(Some("false")));
        assert!(is_env_defined_falsy(Some(" OFF ")));
        assert!(!is_env_defined_falsy(Some("1")));
        assert!(!is_env_defined_falsy(Some("")));
        assert!(!is_env_defined_falsy(None));
    }
}
