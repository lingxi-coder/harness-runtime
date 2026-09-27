//! `WebSearchTool` — routes the agent's query through Anthropic's Messages
//! API with the `web-search-2025-03-05` anthropic-beta header and a
//! `web_search_20250305` tool block. Spec §7 web wire identifiers.
//!
//! Wire-locked constants (asserted byte-for-byte by `parity_web_tools.json`):
//! - `WEB_SEARCH_TOOL_BLOCK_TYPE = "web_search_20250305"` (tool block `type`)
//! - `WEB_SEARCH_TOOL_BLOCK_NAME = "web_search"` (tool block `name`)
//! - `WEB_SEARCH_MAX_USES = 8` (upstream `WebSearchTool.ts:80`)
//! - `WEB_SEARCH_DEFAULT_MAX_TOKENS = 4096`
//! - `anthropic-beta: web-search-2025-03-05` (via `WEB_SEARCH_BETA` local const)

use crate::web_fetch::WEBFETCH_USER_AGENT_PREFIX;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::http::HttpError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{WEB_SEARCH_COMPLETED, WEB_SEARCH_FAILED, WEB_SEARCH_STARTED};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Wire `type` field on the WebSearch tool block. Spec §7 lock; matches
/// `claude-code/src/tools/WebSearchTool/WebSearchTool.ts:78`.
pub const WEB_SEARCH_TOOL_BLOCK_TYPE: &str = "web_search_20250305";

/// Wire `name` field on the WebSearch tool block. Matches upstream.
pub const WEB_SEARCH_TOOL_BLOCK_NAME: &str = "web_search";

/// `max_uses` cap on a single search request. claude-code lock.
pub const WEB_SEARCH_MAX_USES: u32 = 8;

/// `max_tokens` used when WebSearchTool calls `POST /v1/messages`.
pub const WEB_SEARCH_DEFAULT_MAX_TOKENS: u32 = 4096;

/// Canonical tool name in the registry.
pub const TOOL_NAME: &str = "WebSearch";

/// Default session-wide WebSearch budget — claude-code `ifg = 200` (the `??`
/// fallback of `ktu()`).
pub const DEFAULT_MAX_WEB_SEARCHES_PER_SESSION: u32 = 200;

/// `tengu_feature_bad` event name — the generic feature-failure telemetry the
/// binary's `me(e,t,r)` helper emits (`M("tengu_feature_bad",{...r,feature_name:
/// e,error_code:t})`). WebSearch fires it once when the per-session budget is hit.
const TENGU_FEATURE_BAD: &str = "tengu_feature_bad";
/// `feature_name` metadata value on the session-cap `tengu_feature_bad` event
/// (the `me("tool_web_search", …)` first arg).
const WEB_SEARCH_FEATURE_NAME: &str = "tool_web_search";
/// `error_code` metadata value on the session-cap `tengu_feature_bad` event
/// (the `me(…, "web_search_session_cap", …)` second arg).
const WEB_SEARCH_SESSION_CAP_CODE: &str = "web_search_session_cap";

/// Resolve the per-session WebSearch budget — 1:1 with claude-code `ktu()`
/// (`return Z.CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION ?? 200`). The env var is
/// parsed as `Pe.int({ min: 1, digitsOnly: true })`: the trimmed value must be an
/// integer literal AND be `>= 1`; anything else (absent / non-numeric / `< 1`)
/// falls back to the 200 default. An over-`u32` value clamps to `u32::MAX`
/// (effectively unlimited — matching CC's "huge number ⇒ never caps"). The env
/// var keeps its verbatim `CLAUDE_CODE_*` spelling (LingXi retains those).
#[must_use]
pub fn resolve_max_web_searches_per_session() -> u32 {
    parse_max_web_searches(
        std::env::var("CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION")
            .ok()
            .as_deref(),
    )
}

/// Pure core of [`resolve_max_web_searches_per_session`] (testable without env
/// mutation). Mirrors `Pe.int({ min: 1, digitsOnly: true }) ?? 200`.
fn parse_max_web_searches(raw: Option<&str>) -> u32 {
    raw.map(str::trim)
        // `u64::from_str` accepts an optional leading `+` and rejects any
        // non-digit / `-` / decimal-point input — the same set the binary's
        // `^[+-]?\d+$` digitsOnly regex + `parseInt` admits (a `-N` value fails
        // the `min: 1` check there and fails u64 parse here; both ⇒ default).
        .and_then(|s| match s.parse::<u64>() {
            Ok(n) => Some(n),
            // A positive integer literal too large for `u64` (a 20+-digit
            // "effectively unlimited" value) still matches CC's `digitsOnly`
            // regex and yields a huge finite `parseInt` >= 1, so CC never caps.
            // Saturate to `u64::MAX` (→ `u32::MAX` below) rather than failing the
            // parse and regressing to the 200 default.
            Err(e) if *e.kind() == std::num::IntErrorKind::PosOverflow => Some(u64::MAX),
            Err(_) => None,
        })
        .filter(|&n| n >= 1)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        .unwrap_or(DEFAULT_MAX_WEB_SEARCHES_PER_SESSION)
}

/// The session-cap budget notice — 1:1 with the binary's
/// `` `Web search was not performed: this session has used its web search budget
/// (${l} of ${a} WebSearch calls). …` `` where `l` is the current count and `a`
/// the resolved max. Returned verbatim as the single `results` entry.
fn web_search_budget_notice(used: u32, max: u32) -> String {
    format!(
        "Web search was not performed: this session has used its web search budget ({used} of {max} WebSearch calls). Continue with the information already gathered instead of issuing more searches. If more searches are genuinely needed, ask the user to raise CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION."
    )
}

/// `anthropic-beta` value that gates the web-search tool on the Anthropic API.
///
/// Wire-locked byte-for-byte against `claude-code/src/constants/betas.ts`
/// (`WEB_SEARCH = "web-search-2025-03-05"`). Local copy so tools/web does not
/// depend on api-client.
const WEB_SEARCH_BETA: &str = "web-search-2025-03-05";

/// Minimal usage counters decoded from a WebSearch `POST /v1/messages` response.
///
/// Only `input_tokens` and `output_tokens` are needed; other fields on the
/// Anthropic `usage` object are ignored. Matches the two fields `WebSearchTool`
/// read from the two usage fields in the response — ported here so tools/web does not
/// depend on api-client.
///
/// The usage field in `WebSearchMessageResponse` is itself `#[serde(default)]`,
/// which requires `Default`. `input_tokens` and `output_tokens` also carry
/// `#[serde(default)]` so that a partial or missing `usage` object decodes to
/// zeros rather than failing — consistent with the parent field's defaulting
/// contract (unlike the standalone `UsageApi` type in the old api-client, which
/// required both fields).
#[derive(Debug, Default, Clone, serde::Deserialize)]
struct WebSearchUsage {
    /// Number of input tokens billed.
    #[serde(default)]
    input_tokens: u64,
    /// Number of output tokens billed.
    #[serde(default)]
    output_tokens: u64,
}

/// API provider family — the subset of claude-code's `getAPIProvider()` values
/// that `WebSearchTool.isEnabled` branches on (`WebSearchTool.ts:168-193`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiProvider {
    /// Anthropic first-party API (`api.anthropic.com`).
    FirstParty,
    /// Google Vertex AI.
    Vertex,
    /// Azure AI Foundry.
    Foundry,
    /// Anything else (Bedrock, custom gateways, OpenAI-compatible, …).
    Other,
}

/// Infer the [`ApiProvider`] from a request `base_url`. WebSearch routes through
/// `BuiltinToolContext.provider` (an `AnthropicRequestBuilder`) whose `base_url`
/// is the only provider signal reachable from `tools/web` — the typed
/// `getAPIProvider()` value lives in host config, which is out of this crate's
/// scope (a `BuiltinToolContext::api_provider` field would be the faithful source
/// but touches the frozen `tool-api`). Mapping: `api.anthropic.com` ⇒ first-party;
/// a `*.aiplatform.googleapis.com` / `…-aiplatform.…` host ⇒ Vertex; an Azure /
/// Foundry host (`.azure.com`, `cognitiveservices`, `models.ai.azure.com`) ⇒
/// Foundry; everything else ⇒ Other. LingXi's default base_url is
/// `https://api.anthropic.com`, so the default is `FirstParty`.
#[must_use]
pub fn infer_api_provider(base_url: &str) -> ApiProvider {
    let host = url::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    if host == "api.anthropic.com" {
        ApiProvider::FirstParty
    } else if host.contains("aiplatform.googleapis.com") || host.contains("-aiplatform.") {
        ApiProvider::Vertex
    } else if host.ends_with(".azure.com")
        || host.contains("cognitiveservices")
        || host.contains("ai.azure.com")
    {
        ApiProvider::Foundry
    } else {
        ApiProvider::Other
    }
}

/// Whether WebSearch is enabled for `provider` + `model` — 1:1 with claude-code
/// `WebSearchTool.isEnabled` (`WebSearchTool.ts:168-193`):
/// - `firstParty` ⇒ always enabled;
/// - `vertex` ⇒ enabled only for `claude-fable-5-1` or Claude 4.x
///   (`claude-opus-4` / `claude-sonnet-4` / `claude-haiku-4` substring);
/// - `foundry` ⇒ always enabled (Foundry only ships web-search-capable models);
/// - anything else ⇒ disabled.
#[must_use]
pub fn web_search_is_enabled(provider: ApiProvider, model: &str) -> bool {
    match provider {
        // firstParty: any model. foundry: only ships web-search-capable models,
        // so it is likewise unconditionally enabled (TS treats them as separate
        // `if` branches both returning `true` — merged here, same behavior).
        ApiProvider::FirstParty | ApiProvider::Foundry => true,
        ApiProvider::Vertex => {
            model.contains("claude-fable-5-1")
                || model.contains("claude-opus-4")
                || model.contains("claude-sonnet-4")
                || model.contains("claude-haiku-4")
        }
        ApiProvider::Other => false,
    }
}

fn hosted_web_search_enabled(
    model_profile: Option<&str>,
    fallback_provider: ApiProvider,
    model: &str,
) -> bool {
    match model_profile {
        Some("anthropic") => true,
        Some(_) => false,
        // No explicit profile: hosted (Anthropic server-side) web search only
        // exists for CLAUDE models. Gate the fallback on the LIVE model id being
        // a Claude model so a non-Claude model with `model_profile == None`
        // — a resumed cross-provider session (resume clears the profile) or the
        // refusal-fallback swap (`session.model_profile = None`) — does NOT get
        // the hosted framing/call-path. Without this, `fallback_provider`
        // (inferred from the BOOT request base_url, ~always `api.anthropic.com`
        // ⇒ FirstParty) wrongly reports hosted for e.g. a resumed deepseek turn,
        // which then mis-frames WebSearch and attempts the hosted path against a
        // non-Anthropic endpoint. claude-code only runs Claude, so its `None`
        // path always saw a Claude model — byte-identical there.
        None => {
            model.to_ascii_lowercase().contains("claude")
                && web_search_is_enabled(fallback_provider, model)
        }
    }
}

/// Input schema for `WebSearchTool`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchInput {
    /// Search query (>= 2 characters).
    pub query: String,
    /// If present, restrict results to these domains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    /// If present, exclude these domains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_domains: Option<Vec<String>>,
}

/// Build the JSON tool block that goes into `MessageRequest.tools`.
///
/// Wire-locked: `{ "type": "web_search_20250305", "name": "web_search",
/// "max_uses": 8, "allowed_domains"?: [...], "blocked_domains"?: [...] }`.
#[must_use]
pub fn build_tool_block(input: &WebSearchInput) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(WEB_SEARCH_TOOL_BLOCK_TYPE));
    m.insert("name".into(), json!(WEB_SEARCH_TOOL_BLOCK_NAME));
    m.insert("max_uses".into(), json!(WEB_SEARCH_MAX_USES));
    if let Some(a) = &input.allowed_domains {
        m.insert("allowed_domains".into(), json!(a));
    }
    if let Some(b) = &input.blocked_domains {
        m.insert("blocked_domains".into(), json!(b));
    }
    Value::Object(m)
}

/// Build the full `POST /v1/messages` body for WebSearch.
///
/// This is the **non-streaming** body used by the blocking fallback path
/// ([`WebSearchTool::call`] when `stream_sse` is unavailable). The primary path
/// uses [`build_streaming_request_body`], which is identical except for the
/// added `"stream": true`.
/// claude-code's WebSearch inner-query framing (WebSearchTool.ts:258, 270-271):
/// the user message is prefixed with `Perform a web search for the query: ` and
/// a single-line system prompt frames the hosted-search model. The deeper
/// `queryModelWithStreaming` params (thinkingConfig / toolChoice / agents /
/// effortValue) are NOT reachable from this crate (it cannot touch the typed
/// query pipeline — see the module doc) and the hosted-search RESULTS are
/// equivalent regardless, so only the observable framing is mirrored here.
const WEB_SEARCH_USER_PREFIX: &str = "Perform a web search for the query: ";
const WEB_SEARCH_SYSTEM_PROMPT: &str = "You are an assistant for performing a web search tool use";

#[must_use]
pub fn build_request_body(model: &str, input: &WebSearchInput) -> Value {
    json!({
        "model": model,
        "max_tokens": WEB_SEARCH_DEFAULT_MAX_TOKENS,
        "system": WEB_SEARCH_SYSTEM_PROMPT,
        "messages": [
            { "role": "user", "content": format!("{WEB_SEARCH_USER_PREFIX}{}", input.query) }
        ],
        "tools": [ build_tool_block(input) ],
    })
}

/// Build the full `POST /v1/messages` body for WebSearch with **streaming**
/// enabled (`"stream": true`).
///
/// This mirrors claude-code's `queryModelWithStreaming` path: the same request
/// as [`build_request_body`] but asking the server to deliver the response as a
/// Server-Sent-Events message stream so incremental progress (the
/// `server_tool_use` / `web_search_tool_result` blocks) can be observed as the
/// hosted search runs, rather than as one blocking POST.
#[must_use]
pub fn build_streaming_request_body(model: &str, input: &WebSearchInput) -> Value {
    json!({
        "model": model,
        "max_tokens": WEB_SEARCH_DEFAULT_MAX_TOKENS,
        "system": WEB_SEARCH_SYSTEM_PROMPT,
        "messages": [
            { "role": "user", "content": format!("{WEB_SEARCH_USER_PREFIX}{}", input.query) }
        ],
        "tools": [ build_tool_block(input) ],
        "stream": true,
    })
}

/// One parsed search-output entry: either a free-form text segment or a
/// structured search-result hit object.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SearchResultEntry {
    /// Free-form text accumulated from one or more consecutive `text` blocks
    /// (also used for the `Web search error: <code>` string emitted on a
    /// `web_search_tool_result` error payload).
    Text(String),
    /// Structured search result from a `web_search_tool_result` block, shaped
    /// `{ "tool_use_id": <id>, "content": [{ "title", "url" }, ...] }`.
    Hit(Value),
}

/// Minimal view of the `messages` response consumed by WebSearch.
///
/// `content` is intentionally kept as raw `serde_json::Value` blocks rather than
/// a typed content-block enum: the web-search response carries
/// `web_search_tool_result` blocks that `ContentBlockApi` does not model, and
/// adding that variant would break the exhaustive `ContentBlockApi` matches in
/// the agent/orchestrator/sidequery crates. This mirrors how upstream consumes
/// the response as an opaque `BetaContentBlock[]` (`WebSearchTool.ts:86`).
#[derive(Debug, Deserialize)]
struct WebSearchMessageResponse {
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default)]
    usage: WebSearchUsage,
}

/// Walk a response `content` array and produce the search output, mirroring
/// upstream `makeOutputFromSearchResponse` (`WebSearchTool.ts:86-150`)
/// byte-for-byte.
///
/// The block sequence is, repeated per search:
/// `server_tool_use` → `web_search_tool_result` → intermingled `text`/citation
/// blocks. The faithful state machine:
/// - `text` blocks accumulate into a running buffer while `in_text` is set;
/// - a `server_tool_use` block flushes the trimmed buffer (when non-empty) as a
///   [`SearchResultEntry::Text`], clears it, and drops `in_text` — the
///   `server_tool_use` block itself carries only the QUERY and is **never**
///   emitted as a result;
/// - a `web_search_tool_result` block whose `content` is an array emits a
///   [`SearchResultEntry::Hit`] of `{ tool_use_id, content: [{title, url}] }`;
///   when `content` is an error object instead, it emits the
///   `Web search error: <error_code>` string as a [`SearchResultEntry::Text`];
/// - a `text` block seen after a flush starts a fresh buffer.
///
/// Any trailing buffered text is flushed at the end.
#[must_use]
pub fn parse_response_content(content: &[Value]) -> Vec<SearchResultEntry> {
    let mut out: Vec<SearchResultEntry> = Vec::new();
    let mut text_acc = String::new();
    let mut in_text = true;

    for block in content {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "server_tool_use" => {
                if in_text {
                    in_text = false;
                    let trimmed = text_acc.trim();
                    if !trimmed.is_empty() {
                        out.push(SearchResultEntry::Text(trimmed.to_string()));
                    }
                    text_acc.clear();
                }
            }
            "web_search_tool_result" => match block.get("content") {
                // Success case — `content` is an array of search hits.
                Some(Value::Array(items)) => {
                    let hits: Vec<Value> = items
                        .iter()
                        .map(|r| {
                            json!({
                                "title": r.get("title").cloned().unwrap_or(Value::Null),
                                "url": r.get("url").cloned().unwrap_or(Value::Null),
                            })
                        })
                        .collect();
                    out.push(SearchResultEntry::Hit(json!({
                        "tool_use_id": block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                        "content": hits,
                    })));
                }
                // Error case — `content` is a `WebSearchToolResultError`.
                other => {
                    let code = other.and_then(|c| c.get("error_code")).map_or_else(
                        || "undefined".to_string(),
                        |v| match v {
                            Value::String(s) => s.clone(),
                            _ => v.to_string(),
                        },
                    );
                    out.push(SearchResultEntry::Text(format!("Web search error: {code}")));
                }
            },
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                if in_text {
                    text_acc.push_str(text);
                } else {
                    in_text = true;
                    text_acc = text.to_string();
                }
            }
            _ => {}
        }
    }

    // Flush any trailing buffered text (`if (textAcc.length)` in upstream).
    if !text_acc.is_empty() {
        out.push(SearchResultEntry::Text(text_acc.trim().to_string()));
    }
    out
}

/// Count the number of web searches performed in a response `content` array —
/// 1:1 with claude-code `V7p` (binary @~148664): `searchCount = Math.max(i, a)`
/// where `i` is the number of `server_tool_use` blocks (each a hosted search
/// invocation) and `a` is the number of `web_search_tool_result` blocks (each a
/// search result, including error payloads). Emitted into the result `data` as
/// the `searchCount` field (`outputSchema` `searchCount: A.number().optional()`,
/// "Number of web searches performed").
#[must_use]
pub fn count_searches(content: &[Value]) -> u64 {
    let mut server_tool_use = 0u64;
    let mut tool_result = 0u64;
    for block in content {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "server_tool_use" => server_tool_use += 1,
            "web_search_tool_result" => tool_result += 1,
            _ => {}
        }
    }
    server_tool_use.max(tool_result)
}

/// The model-visible incomplete-response notice appended to the salvaged content
/// when a hosted WebSearch message stream is cut off mid-response. CC 2.1.207's
/// WebSearch tool has no bespoke partial handler — it inherits the query-loop
/// partial finalize, which yields an `API Error: …` text block into the
/// accumulated content (`p.push(...v.message.content)`), so the received search
/// results survive with this notice appended. The exact text is selected by the
/// cutoff cause, byte-locked to the finalize strings.
fn stream_partial_notice(err: &HttpError) -> &'static str {
    match err {
        HttpError::Timeout(_) => {
            "API Error: The response stopped arriving. The response above may be incomplete."
        }
        HttpError::Status { .. } | HttpError::InvalidResponse(_) => {
            "API Error: Server error mid-response. The response above may be incomplete."
        }
        _ => "API Error: Connection lost mid-response. The response above may be incomplete.",
    }
}

/// Streaming-SSE block reassembler — rebuilds the raw `Vec<serde_json::Value>`
/// content-block array from an Anthropic message-stream event sequence so it can
/// be fed to the EXISTING [`parse_response_content`] verbatim.
///
/// This is the streaming analogue of decoding a blocking `messages` response's
/// `content[]`. claude-code's `queryModelWithStreaming` accumulates
/// `BetaContentBlock[]` from the same events (`WebSearchTool.ts:293-388`); here
/// we reassemble the blocks into the identical raw-JSON shape rather than a
/// typed enum, because WebSearch's `web_search_tool_result` / `server_tool_use`
/// blocks are not modelled by the engine's typed content-block enum (the
/// orchestrator's typed accumulator collapses them to `Skipped`).
///
/// Handled events (Anthropic documented message-stream shape):
/// - `content_block_start { index, content_block }` → store the block's initial
///   JSON at `index` (e.g. `{type:"server_tool_use",id,name,input:{}}`,
///   `{type:"web_search_tool_result",tool_use_id,content:[...]}`,
///   `{type:"text",text:""}`).
/// - `content_block_delta { index, delta }`:
///   - `text_delta` → append `delta.text` to `block[index].text`;
///   - `input_json_delta` → append `delta.partial_json` to a per-index buffer;
///   - `citations_delta` → push `delta.citation` into `block[index].citations`.
/// - `content_block_stop { index }` → if an `input_json` buffer accumulated for
///   `index`, parse it and set `block[index].input`.
/// - `message_start { message: { usage } }` / `message_delta { usage }` →
///   capture `input_tokens` / `output_tokens` for telemetry parity with the
///   blocking path's `usage` object.
/// - `message_stop` → terminal (the stream also ends naturally).
#[derive(Debug, Default)]
struct StreamReassembler {
    /// Reassembled blocks keyed by their stream `index`. A `BTreeMap` so the
    /// final `into_blocks()` yields blocks in ascending index order — the same
    /// order they appear in a blocking response's `content[]`.
    blocks: std::collections::BTreeMap<u64, Value>,
    /// Per-index `input_json_delta` accumulators (raw `partial_json` concat),
    /// finalized into `block[index].input` on `content_block_stop`.
    json_bufs: std::collections::BTreeMap<u64, String>,
    /// Usage counters captured from `message_start` / `message_delta`, mirroring
    /// the two fields the blocking path read off the response `usage` object.
    usage: WebSearchUsage,
}

impl StreamReassembler {
    /// Apply one decoded message-stream event (the parsed `SseEvent.data` JSON).
    ///
    /// Returns `true` when this event is a `content_block_start` for a
    /// `server_tool_use` block — the caller uses that signal to emit a
    /// "searching" progress event (mirrors claude-code's per-search progress on
    /// the server-tool-use start, `WebSearchTool.ts:306-318`).
    fn apply(&mut self, event: &Value) -> bool {
        let ev_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        match ev_type {
            "message_start" => {
                if let Some(u) = event.get("message").and_then(|m| m.get("usage")) {
                    self.capture_usage(u);
                }
                false
            }
            "message_delta" => {
                // `message_delta` carries a top-level `usage` with the running
                // `output_tokens` (and sometimes refined `input_tokens`).
                if let Some(u) = event.get("usage") {
                    self.capture_usage(u);
                }
                false
            }
            "content_block_start" => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    return false;
                };
                let block = event.get("content_block").cloned().unwrap_or(json!({}));
                let is_server_tool_use =
                    block.get("type").and_then(Value::as_str) == Some("server_tool_use");
                self.blocks.insert(index, block);
                // A fresh block starts an empty JSON buffer slot; `input_json_delta`
                // chunks append to it and `content_block_stop` finalizes it.
                self.json_bufs.insert(index, String::new());
                is_server_tool_use
            }
            "content_block_delta" => {
                let Some(index) = event.get("index").and_then(Value::as_u64) else {
                    return false;
                };
                if let Some(delta) = event.get("delta") {
                    self.apply_delta(index, delta);
                }
                false
            }
            "content_block_stop" => {
                if let Some(index) = event.get("index").and_then(Value::as_u64) {
                    self.finalize_block(index);
                }
                false
            }
            // `message_stop`, `ping`, and any other events carry no block data.
            _ => false,
        }
    }

    /// Apply a single `content_block_delta`'s `delta` payload to block `index`.
    fn apply_delta(&mut self, index: u64, delta: &Value) {
        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" => {
                let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                let block = self.blocks.entry(index).or_insert_with(|| json!({}));
                append_str_field(block, "text", text);
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.json_bufs.entry(index).or_default().push_str(partial);
            }
            "citations_delta" => {
                if let Some(citation) = delta.get("citation") {
                    let block = self.blocks.entry(index).or_insert_with(|| json!({}));
                    push_array_field(block, "citations", citation.clone());
                }
            }
            // `signature_delta`, `thinking_delta`, etc. are irrelevant to the
            // WebSearch block types `parse_response_content` consumes.
            _ => {}
        }
    }

    /// Finalize block `index` on `content_block_stop`: if an `input_json` buffer
    /// accumulated, parse it and set `block[index].input` (the reassembled
    /// `server_tool_use` query lives here). A non-empty buffer that fails to
    /// parse is left unset — `parse_response_content` never reads `input`, so
    /// this only affects faithfulness of the reassembled block, not the result.
    fn finalize_block(&mut self, index: u64) {
        let buf = self.json_bufs.remove(&index).unwrap_or_default();
        if buf.is_empty() {
            return;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(&buf) {
            if let Some(block) = self.blocks.get_mut(&index) {
                if let Some(obj) = block.as_object_mut() {
                    obj.insert("input".to_string(), parsed);
                }
            }
        }
    }

    /// Capture `input_tokens` / `output_tokens` from a usage object, taking the
    /// max seen so far (later `message_delta` usages refine `output_tokens`).
    fn capture_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.usage.input_tokens = self.usage.input_tokens.max(v);
        }
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = self.usage.output_tokens.max(v);
        }
    }

    /// Consume the reassembler into the ordered raw content-block array (ascending
    /// `index`) ready for [`parse_response_content`].
    fn into_blocks(self) -> Vec<Value> {
        self.blocks.into_values().collect()
    }
}

/// Append `text` to `block[field]` (a string), creating the field if absent.
fn append_str_field(block: &mut Value, field: &str, text: &str) {
    if let Some(obj) = block.as_object_mut() {
        let existing = obj.get(field).and_then(Value::as_str).unwrap_or("");
        let combined = format!("{existing}{text}");
        obj.insert(field.to_string(), Value::String(combined));
    }
}

/// Push `item` onto `block[field]` (a JSON array), creating the array if absent.
fn push_array_field(block: &mut Value, field: &str, item: Value) {
    if let Some(obj) = block.as_object_mut() {
        match obj.get_mut(field).and_then(Value::as_array_mut) {
            Some(arr) => arr.push(item),
            None => {
                obj.insert(field.to_string(), Value::Array(vec![item]));
            }
        }
    }
}

/// Build the model-facing text block for a successful search, mirroring
/// upstream `mapToolResultToToolResultBlockParam` (`WebSearchTool.ts:401`)
/// byte-for-byte: a `Web search results for query: "<q>"` header, one
/// rendered segment per result entry, and a trailing `REMINDER:` footer,
/// with the whole string `.trim()`-ed.
///
/// Per-entry rendering follows the TS branches:
/// - a text segment is appended verbatim plus a blank line;
/// - a hit object renders `Links: <json>` when it carries a non-empty
///   `content` array, otherwise `No links found.`. After WEB.4,
///   [`SearchResultEntry::Hit`] is `{tool_use_id, content:[{title,url}]}`, so
///   `content` is the hits array serialized verbatim into the `Links:` line.
#[must_use]
pub fn build_model_content(query: &str, results: &[SearchResultEntry]) -> String {
    let mut out = format!("Web search results for query: \"{query}\"\n\n");
    for entry in results {
        match entry {
            SearchResultEntry::Text(s) => {
                out.push_str(s);
                out.push_str("\n\n");
            }
            SearchResultEntry::Hit(v) => match v.get("content").and_then(Value::as_array) {
                Some(arr) if !arr.is_empty() => {
                    let rendered = serde_json::to_string(arr).unwrap_or_default();
                    out.push_str(&format!("Links: {rendered}\n\n"));
                }
                _ => out.push_str("No links found.\n\n"),
            },
        }
    }
    out.push_str(
        "\nREMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.",
    );
    out.trim().to_string()
}

/// Build the budget-capped [`ToolCallResult`] returned WITHOUT searching once the
/// session WebSearch budget is exhausted — 1:1 with the binary's
/// `{data:{query,results:[<notice>],durationSeconds:0,searchCount:0}}` return.
/// The model-facing text runs the single-entry notice through
/// [`build_model_content`] (the binary's `mapToolResultToToolResultBlockParam`,
/// which wraps every result set in the `Web search results for query: "<q>"`
/// header + cite-sources footer). `is_error` stays `false`: CC returns the cap as
/// a normal (non-thrown) tool result so the model reads the notice and stops.
fn budget_capped_result(query: &str, used: u32, max: u32) -> ToolCallResult {
    let notice = web_search_budget_notice(used, max);
    let model_content = build_model_content(query, &[SearchResultEntry::Text(notice.clone())]);
    ToolCallResult {
        data: json!({
            "query": query,
            "results": [notice],
            "durationSeconds": 0,
            "searchCount": 0,
        }),
        model_content: Some(model_content),
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

/// `WebSearchTool` — routes the agent's query through Anthropic's Messages
/// API with `anthropic-beta: web-search-2025-03-05` and the
/// `web_search_20250305` tool block. Never self-retries.
pub struct WebSearchTool {
    ctx: BuiltinToolContext,
}

impl WebSearchTool {
    /// Construct a new tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    fn user_agent() -> String {
        format!(
            "{}{}",
            WEBFETCH_USER_AGENT_PREFIX,
            env!("CARGO_PKG_VERSION")
        )
    }

    async fn emit_started(
        &self,
        invocation_id: &str,
        query: &str,
        allowed_count: usize,
        blocked_count: usize,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "_PROTO_query".into(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(query.to_string()).into_inner(),
            ),
        );
        md.insert(
            "allowed_domains_count".into(),
            AnalyticsValue::Int(allowed_count as i64),
        );
        md.insert(
            "blocked_domains_count".into(),
            AnalyticsValue::Int(blocked_count as i64),
        );
        self.ctx.bus.log_event(WEB_SEARCH_STARTED, md).await;
    }

    async fn emit_completed(
        &self,
        invocation_id: &str,
        hits: u64,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert("hits".into(), AnalyticsValue::Int(hits as i64));
        md.insert(
            "input_tokens".into(),
            AnalyticsValue::Int(input_tokens as i64),
        );
        md.insert(
            "output_tokens".into(),
            AnalyticsValue::Int(output_tokens as i64),
        );
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(WEB_SEARCH_COMPLETED, md).await;
    }

    async fn emit_failed(
        &self,
        invocation_id: &str,
        error_kind: &str,
        status: Option<u16>,
        duration_ms: u64,
    ) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "error_kind".into(),
            AnalyticsValue::String(Verified::assert_safe(error_kind.to_string()).into_inner()),
        );
        if let Some(s) = status {
            md.insert("status".into(), AnalyticsValue::Int(i64::from(s)));
        }
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(WEB_SEARCH_FAILED, md).await;
    }

    /// Emit the session-cap `tengu_feature_bad` event — 1:1 with the binary's
    /// `me("tool_web_search","web_search_session_cap",{max_web_searches_per_session:a})`
    /// (`me(e,t,r) = M("tengu_feature_bad",{...r,feature_name:e,error_code:t})`).
    /// Fired once, immediately before the budget notice is returned.
    async fn emit_web_search_session_cap(&self, max: u32) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "feature_name".into(),
            AnalyticsValue::String(
                Verified::assert_safe(WEB_SEARCH_FEATURE_NAME.to_string()).into_inner(),
            ),
        );
        md.insert(
            "error_code".into(),
            AnalyticsValue::String(
                Verified::assert_safe(WEB_SEARCH_SESSION_CAP_CODE.to_string()).into_inner(),
            ),
        );
        md.insert(
            "max_web_searches_per_session".into(),
            AnalyticsValue::Int(i64::from(max)),
        );
        self.ctx.bus.log_event(TENGU_FEATURE_BAD, md).await;
    }

    /// Provider-agnostic client-side search path (non-Anthropic providers).
    /// Runs the search over `ctx.http` and returns markdown result blocks.
    async fn run_client_side(&self, input: &WebSearchInput) -> Result<ToolCallResult, ToolError> {
        use crate::web_search_client::{
            format_results_for_model, resolve_client_search_provider_with_credentials,
            run_client_web_search, ClientSearchProvider, EnvSearchConfig, ResolvedWebCredentials,
        };
        use crate::web_search_config::WebSearchConfig;
        let invocation_id = tool_api::util::ids::ulid_or_uuid();
        let allowed = input.allowed_domains.clone().unwrap_or_default();
        let blocked = input.blocked_domains.clone().unwrap_or_default();
        self.emit_started(&invocation_id, &input.query, allowed.len(), blocked.len())
            .await;
        let started = Instant::now();
        let (web_cfg, providers) = if let Some(loader) = &self.ctx.web_search_config {
            let cfg = loader.load_web_search_config().await;
            let web_cfg = WebSearchConfig {
                provider: cfg
                    .provider
                    .as_deref()
                    .and_then(crate::web_search_config::WebSearchProvider::parse)
                    .unwrap_or(crate::web_search_config::WebSearchProvider::Auto),
                searxng_url: cfg.searxng_url,
            };
            let creds = ResolvedWebCredentials {
                tavily_key: cfg.tavily_key,
                brave_key: cfg.brave_key,
            };
            let env = EnvSearchConfig::from_env();
            let providers = if web_cfg.provider == crate::web_search_config::WebSearchProvider::Auto
            {
                crate::web_search_client::resolve_client_search_candidates(&web_cfg, &creds, &env)
            } else {
                resolve_client_search_provider_with_credentials(&web_cfg, &creds, &env)
                    .map(|p| vec![p])
                    .unwrap_or_else(|_| vec![ClientSearchProvider::DuckDuckGo])
            };
            (web_cfg, providers)
        } else {
            let web_cfg = WebSearchConfig::default();
            let creds = ResolvedWebCredentials::from_env();
            let env = EnvSearchConfig::from_env();
            (
                web_cfg.clone(),
                crate::web_search_client::resolve_client_search_candidates(&web_cfg, &creds, &env),
            )
        };
        let mut last_error = None;
        let mut success = None;
        for provider in providers {
            let label = provider.label();
            match run_client_web_search(
                &self.ctx.http,
                &provider,
                &input.query,
                &allowed,
                &blocked,
                0,
            )
            .await
            {
                Ok(hits) => {
                    success = Some((provider, hits));
                    break;
                }
                Err(msg) => {
                    last_error = Some(format!("{label}: {msg}"));
                    if web_cfg.provider != crate::web_search_config::WebSearchProvider::Auto {
                        break;
                    }
                }
            }
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match success {
            Some((provider, hits)) => {
                self.emit_completed(&invocation_id, hits.len() as u64, 0, 0, elapsed_ms)
                    .await;
                let model_content = format_results_for_model(&input.query, &hits, provider.label());
                Ok(ToolCallResult {
                    data: serde_json::json!({
                        "query": input.query,
                        "provider": provider.label(),
                        "result_count": hits.len(),
                    }),
                    model_content: Some(model_content),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: false,
                    mcp_meta: None,
                })
            }
            None => {
                let msg = last_error.unwrap_or_else(|| {
                    "No web search provider is configured and DuckDuckGo fallback was unavailable"
                        .to_string()
                });
                self.emit_failed(&invocation_id, "client_search", None, elapsed_ms)
                    .await;
                Ok(ToolCallResult {
                    data: serde_json::json!({ "query": input.query, "error": msg }),
                    model_content: Some(msg),
                    new_messages: vec![],
                    context_modifier: None,
                    is_error: true,
                    mcp_meta: None,
                })
            }
        }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["query"],
        "properties": {
            "query": { "type": "string", "minLength": 2, "description": "The search query to use" },
            "allowed_domains": { "type": "array", "items": { "type": "string" }, "description": "Only include search results from these domains" },
            "blocked_domains": { "type": "array", "items": { "type": "string" }, "description": "Never include search results from these domains" }
        }
    })
});

/// Verbatim claude-code v2.1.181 WebSearch tool description. The "current month"
/// is a RUNTIME slot (claude-code computes it at render time); the format is
/// "<Month> <Year>" (e.g. "June 2026"), matching the knowledge-cutoff date style
/// — the only non-static byte in the whole string.
fn web_search_description() -> String {
    let month_year = chrono::Local::now().format("%B %Y").to_string();
    // RAW string with real newlines + indentation: a `\n\` line continuation
    // strips the leading "  " of each bullet, so it must NOT be used here. The
    // content is flush-left in the source so the only indentation is the text's.
    // The binary's DESCRIPTION template literal begins with a leading newline and
    // prefixes EVERY bullet with `- ` (incl. the first): `\n- Allows Claude...`.
    // It also ends with a trailing `\n`. Both are reproduced here (raw string
    // opens with a newline, first line is `- Allows...`, closes after a newline).
    format!(
        r#"
- Allows Claude to search the web and use the results to inform responses
- Provides up-to-date information for current events and recent data
- Returns search result information formatted as search result blocks, including links as markdown hyperlinks
- Use this tool for accessing information beyond Claude's knowledge cutoff
- Searches are performed automatically within a single API call

CRITICAL REQUIREMENT - You MUST follow this:
  - After answering the user's question, you MUST include a "Sources:" section at the end of your response
  - In the Sources section, list all relevant URLs from the search results as markdown hyperlinks: [Title](URL)
  - This is MANDATORY - never skip including sources in your response
  - Example format:

    [Your answer here]

    Sources:
    - [Source Title 1](https://example.com/1)
    - [Source Title 2](https://example.com/2)

Usage notes:
  - Domain filtering is supported to include or block specific websites
  - Web search is only available in the US

IMPORTANT - Use the correct year in search queries:
  - The current month is {month_year}. You MUST use this year when searching for recent information, documentation, or current events.
  - Example: If the user asks for "latest React docs", search for "React documentation" with the current year, NOT last year
"#
    )
}

/// CONCISE WebSearch prompt — the `Dh(model)`-true branch of claude-code's
/// `CNi(model)` (binary offset ~197074996). Extracted verbatim from the binary:
///
/// ```text
/// Search the web. Returns result blocks with titles and URLs. US-only.
///
/// - The current month is ${t} — use this when searching for recent information.
/// - `allowed_domains` / `blocked_domains` filter results.
/// - After answering from results, end with a "Sources:" list of the URLs you used as markdown links.
/// ```
///
/// `${t}` is `U1i()` = `new Date().toLocaleString("en-US",{month:"long",year:
/// "numeric"})` (binary offset 197026096) → the same `%B %Y` ("June 2026") slot
/// the verbose path uses. The dash is the em-dash (`—`, `—`). Unlike the
/// VERBOSE variant this has NO leading and NO trailing newline. RAW Rust string;
/// the only non-static byte is the `{month_year}` slot.
fn web_search_description_concise() -> String {
    let month_year = chrono::Local::now().format("%B %Y").to_string();
    format!(
        r#"Search the web. Returns result blocks with titles and URLs. US-only.

- The current month is {month_year} — use this when searching for recent information.
- `allowed_domains` / `blocked_domains` filter results.
- After answering from results, end with a "Sources:" list of the URLs you used as markdown links."#
    )
}

/// Select the WebSearch prompt variant — 1:1 with claude-code `CNi(model)`
/// (binary offset ~197074952): `Dh(model) ? CONCISE : VERBOSE`. The session /
/// subagent model is threaded via [`PromptOptions::model`]; `None` mirrors the
/// binary's `Dh(undefined)` → VERBOSE.
///
/// The `Dh(model)` "simple system prompt" gate is the shared
/// [`tool_api::dh_simple_system_prompt`] (single source of truth in
/// `tool-api/src/model_prompt_gate.rs`), consulted identically by the file/task
/// tools — `UWu`/`dfe`/`FWu` parity notes live there.
fn select_web_search_prompt(model: Option<&str>) -> String {
    if tool_api::dh_simple_system_prompt(model) {
        web_search_description_concise()
    } else {
        web_search_description()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("search the web for current information")
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // Always offered: on Anthropic first-party (and Vertex/Foundry per
        // `hosted_search_enabled`) `call` runs the hosted `web_search_20250305`
        // tool; on every other provider it runs the provider-agnostic
        // CLIENT-SIDE search (see `web_search_client`). The provider split lives
        // in `call`, so the model always sees a WebSearch tool.
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }
    fn is_open_world(&self, _input: &Value) -> bool {
        true
    }
    fn interrupt_behavior(&self, _input: &Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-03 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), tool_api::tool_trait::ValidationError> {
        // Binary `validateInput`: `if(!t.length) return {message:"Error: Missing
        // query",errorCode:1}`. The min-2 constraint is schema-enforced
        // (`A.string().min(2)` == the "minLength":2 in this tool's input schema),
        // NOT a validateInput message — the manual <2 guard below is belt-and-
        // suspenders and keeps the port safe if the schema is not pre-validated.
        let q = input.get("query").and_then(Value::as_str).unwrap_or("");
        if q.is_empty() {
            return Err(tool_api::tool_trait::ValidationError(
                "Error: Missing query".into(),
            ));
        }
        if q.chars().count() < 2 {
            return Err(tool_api::tool_trait::ValidationError(
                "query must be at least 2 characters".into(),
            ));
        }
        // Mirror upstream `validateInput` (`WebSearchTool.ts:244`, errorCode 2):
        // reject when BOTH allowed_domains and blocked_domains are non-empty.
        let allowed_non_empty = input
            .get("allowed_domains")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        let blocked_non_empty = input
            .get("blocked_domains")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        if allowed_non_empty && blocked_non_empty {
            return Err(tool_api::tool_trait::ValidationError(
                "Error: Cannot specify both allowed_domains and blocked_domains in the same request"
                    .into(),
            ));
        }
        Ok(())
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        // claude-code's WebSearch tool object has NO model-gated `description`
        // method — only `async prompt({model:e}){return CNi(e)}` carries the
        // `Dh(model)` CONCISE/VERBOSE gate (binary @202215633), exactly as
        // TodoWrite's `description(){return Qla}` is fixed while its `prompt`
        // gates on the model. `DescriptionOptions` carries no model id here, so
        // this returns the VERBOSE variant (== the `Dh(None)`-false default).
        web_search_description()
    }
    async fn prompt(&self, opts: &PromptOptions) -> String {
        // On non-hosted providers WebSearch runs CLIENT-SIDE (see `call` /
        // `web_search_client`), so advertise it as a plain, callable web search
        // instead of the Anthropic-hosted framing ("automatic", "US only") that
        // would otherwise make the model think it can't invoke it.
        if !hosted_web_search_enabled(
            opts.model_profile.as_deref(),
            infer_api_provider(&self.ctx.provider.base_url),
            // LIVE session model (falls back to the boot default only when the
            // caller didn't thread one) — see `hosted_web_search_enabled`.
            opts.model.as_deref().unwrap_or(&self.ctx.default_model),
        ) {
            return "Search the web and return result blocks (title + URL + snippet) as \
                    markdown links. Use this whenever you need up-to-date or real-time \
                    information you don't already know \u{2014} current events, weather, \
                    prices, release notes, documentation, or anything past your training \
                    cutoff. After answering, end with a \"Sources:\" list of the URLs you used."
                .to_string();
        }
        // 1:1 with claude-code `async prompt({model:e}){return CNi(e)}`
        // (binary @202215633), where `CNi(model)=Dh(model)?CONCISE:VERBOSE`
        // (@~197074952). The session/subagent model is threaded via
        // `PromptOptions::model`; `None` ⇒ `Dh(undefined)` ⇒ VERBOSE.
        select_web_search_prompt(opts.model.as_deref())
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let parsed_input: WebSearchInput = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("invalid input: {e}")))?;
        // Binary `validateInput`: empty query -> "Error: Missing query"; min-2 is
        // schema-enforced (belt-and-suspenders guard retained).
        if parsed_input.query.is_empty() {
            return Err(ToolError::InvalidInput("Error: Missing query".into()));
        }
        if parsed_input.query.chars().count() < 2 {
            return Err(ToolError::InvalidInput(
                "query must be at least 2 characters".into(),
            ));
        }

        // Session-wide WebSearch budget (parity 2.1.212): claude-code reads the
        // session counter off the task registry BEFORE every search
        // (`l=t.taskRegistry.getWebSearchCalls(); if(l>=a) return <notice>`); on
        // `>= max` (default 200, `CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION`) it
        // returns a budget notice WITHOUT searching, else increments the counter
        // (`incrementWebSearchCalls()`) and proceeds. Placed BEFORE the provider
        // split so it gates BOTH the hosted and the LingXi client-side paths (1:1
        // with CC where the gate is the first logic in `call`). No registry wired
        // (`None`, e.g. library/unit callers) ⇒ never caps — byte-identical to the
        // binary's null-registry `getWebSearchCalls(){return 0}` stub.
        if let Some(registry) = &self.ctx.task_registry {
            let max = resolve_max_web_searches_per_session();
            let used = registry.web_search_calls();
            if used >= max {
                self.emit_web_search_session_cap(max).await;
                return Ok(budget_capped_result(&parsed_input.query, used, max));
            }
            registry.increment_web_search_calls();
        }

        // Provider split: Anthropic-hosted search only works on the first-party
        // (and Vertex/Foundry) API. On every other provider, run the
        // provider-agnostic CLIENT-SIDE search instead of the hosted tool.
        if !hosted_web_search_enabled(
            ctx.options.model_profile.as_deref(),
            infer_api_provider(&self.ctx.provider.base_url),
            // LIVE main-loop model driving this call — see
            // `hosted_web_search_enabled` (not the boot `ctx.default_model`).
            // Falls back to the boot default only when the caller didn't populate
            // one (library/unit callers with a default `ToolUseContext`).
            if ctx.options.main_loop_model.is_empty() {
                self.ctx.default_model.as_str()
            } else {
                ctx.options.main_loop_model.as_str()
            },
        ) {
            return self.run_client_side(&parsed_input).await;
        }

        let invocation_id = tool_api::util::ids::ulid_or_uuid();
        let allowed_count = parsed_input.allowed_domains.as_ref().map_or(0, Vec::len);
        let blocked_count = parsed_input.blocked_domains.as_ref().map_or(0, Vec::len);
        self.emit_started(
            &invocation_id,
            &parsed_input.query,
            allowed_count,
            blocked_count,
        )
        .await;

        let started = Instant::now();

        // PRIMARY PATH — stream the same `/v1/messages` request with
        // `"stream": true` and reassemble the content blocks incrementally, so
        // search progress is observable as the hosted search runs (1:1 with
        // claude-code's `queryModelWithStreaming`, `WebSearchTool.ts:268-388`).
        // The streaming body's tool block / beta header / model are identical to
        // the blocking body.
        let stream_body = build_streaming_request_body(&self.ctx.default_model, &parsed_input);

        // Bounded 529/overloaded retry around the streaming connect (parity
        // 2.1.212): CC routes this hosted `web_search_tool` query through the
        // retrying `queryModelWithStreaming` wrapper — `initialConsecutive529Errors`
        // + `subscribeRetry`/`onRetryStatus`, where the retry predicate `dNe` is
        // `status===529 || error==="overloaded_error"` and the backoff `sle` is
        // `min(500*2^(n-1), 32000)ms`. lingxi's HttpError collapses the overloaded
        // signal onto HTTP 529, so a connect-phase 529 is retried in place (fresh
        // request per attempt) up to `LINGXI_MAX_RETRIES` (default 10) before
        // falling through to the existing blocking fallback. Non-529 connect errors
        // and a successful connect are unchanged.
        let max_retries = web_search_max_529_retries();
        let mut attempt: u32 = 0;
        loop {
            let stream_req = self.build_messages_request(&stream_body);
            match self.ctx.http.stream_sse(stream_req).await {
                Ok(stream) => {
                    // Connected — drive the SSE stream to completion, reassembling
                    // the raw content-block array and emitting progress as blocks
                    // start. A mid-stream transport error aborts the search.
                    let (blocks, usage) =
                        match Self::consume_stream(stream, &parsed_input.query, &ctx, &tx).await {
                            Ok(out) => out,
                            Err(err) => {
                                let elapsed_ms = started.elapsed().as_millis() as u64;
                                return Err(self
                                    .map_stream_error(&invocation_id, err, elapsed_ms)
                                    .await);
                            }
                        };
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let search_count = count_searches(&blocks);
                    let results = parse_response_content(&blocks);
                    return Ok(self
                        .build_success_result(
                            &invocation_id,
                            &parsed_input.query,
                            results,
                            search_count,
                            usage.input_tokens,
                            usage.output_tokens,
                            elapsed_ms,
                        )
                        .await);
                }
                // RETRY ARM — a connect-phase 529/overloaded, with attempts left:
                // wait out the `sle` exponential backoff and re-issue the stream
                // request (mirrors CC's `queryModelWithStreaming` 529 retry loop).
                Err(err) if is_overloaded_status(&err) && attempt < max_retries => {
                    attempt += 1;
                    tokio::time::sleep(retry_backoff_529(attempt)).await;
                    continue;
                }
                // FALLBACK PATH — `stream_sse` failed at connect (e.g. a transport
                // or test mock that does not implement SSE, or a 529 whose retries
                // are exhausted). Fall back to the original blocking POST +
                // `parse_response_content`, keeping non-SSE transports and the
                // existing blocking tests working. The functional result is
                // identical; only the incremental progress is lost.
                Err(_connect_err) => {
                    let body = build_request_body(&self.ctx.default_model, &parsed_input);
                    let req = self.build_messages_request(&body);
                    let resp_result = self.ctx.http.request(req).await;
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    return self
                        .finish_blocking(
                            &invocation_id,
                            &parsed_input.query,
                            resp_result,
                            elapsed_ms,
                        )
                        .await;
                }
            }
        }
    }
}

/// `dNe` retry predicate: HTTP 529 is lingxi's `HttpError` embodiment of CC's
/// `status===529 || error==="overloaded_error"` overloaded signal (the transport
/// maps the Anthropic `overloaded_error` body to a 529 status).
fn is_overloaded_status(err: &HttpError) -> bool {
    matches!(err, HttpError::Status { status: 529, .. })
}

/// `sle` exponential backoff: `min(500 * 2^(attempt-1), 32000)` ms, `attempt`
/// 1-indexed. CC's `queryModelWithStreaming` applies jitter on top; it is omitted
/// here so the (test-observable) delay is deterministic. The shift is clamped so
/// `1 << shift` never overflows before the `min(…, 32_000)` cap applies.
fn retry_backoff_529(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(20);
    let base = 500u64.saturating_mul(1u64 << shift);
    Duration::from_millis(base.min(32_000))
}

/// `DEFAULT_MAX_RETRIES = 10`, overridable via `LINGXI_MAX_RETRIES` (a trimmed
/// integer literal; anything else falls back to the default) — mirrors CC's
/// `maxRetries` default on the streaming query wrapper.
fn web_search_max_529_retries() -> u32 {
    std::env::var("LINGXI_MAX_RETRIES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(10)
}

impl WebSearchTool {
    /// Assemble a `POST /v1/messages` request for `body` with the WebSearch
    /// `anthropic-beta` + `user-agent` headers attached. Shared by the streaming
    /// and blocking-fallback paths so both send byte-identical headers.
    fn build_messages_request(&self, body: &Value) -> protocol::HttpRequest {
        let mut req = self.ctx.provider.build_request(body);
        req.headers
            .push(("anthropic-beta".into(), WEB_SEARCH_BETA.to_string()));
        req.headers.push(("user-agent".into(), Self::user_agent()));
        req
    }

    /// Drive an SSE message stream to completion, reassembling the raw
    /// content-block array via [`StreamReassembler`] and emitting an incremental
    /// "searching" progress event each time a `server_tool_use` block starts.
    ///
    /// Returns the ordered `Vec<Value>` content blocks (feed straight to
    /// [`parse_response_content`]) plus the captured usage counters. A mid-stream
    /// transport error AFTER at least one block was received is SALVAGED — the
    /// received blocks are returned with an incomplete-response notice text block
    /// appended (CC 2.1.207 partial preservation), so the search result survives.
    /// An error before ANY block is received is surfaced as `Err(HttpError)`.
    async fn consume_stream(
        mut stream: platform_api::http::SseStream,
        query: &str,
        ctx: &ToolUseContext,
        tx: &ToolProgressSender,
    ) -> Result<(Vec<Value>, WebSearchUsage), HttpError> {
        use futures_util::StreamExt;

        let mut acc = StreamReassembler::default();
        let mut progress_counter: u64 = 0;
        let mut saw_message_stop = false;
        while let Some(item) = stream.next().await {
            let ev = match item {
                Ok(ev) => ev,
                Err(err) => {
                    // CC 2.1.207 partial preservation: if any content block was
                    // already received, KEEP it — append the incomplete-response
                    // notice text block and let `parse_response_content` build a
                    // normal (non-error) result, mirroring the query-loop finalize
                    // the WebSearch tool inherits. Nothing received yet ⇒ propagate
                    // the hard error unchanged (an empty transcript is not
                    // recoverable).
                    if acc.blocks.is_empty() {
                        return Err(err);
                    }
                    let usage = acc.usage.clone();
                    let mut blocks = acc.into_blocks();
                    blocks.push(json!({ "type": "text", "text": stream_partial_notice(&err) }));
                    return Ok((blocks, usage));
                }
            };
            // Each SSE frame's `data` is one Anthropic message-stream event JSON.
            // A frame whose data is not parseable JSON (e.g. a `[DONE]` sentinel
            // or a comment) carries no block data and is skipped.
            let Ok(event) = serde_json::from_str::<Value>(&ev.data) else {
                continue;
            };
            let server_tool_use_started = acc.apply(&event);
            if server_tool_use_started {
                progress_counter += 1;
                Self::emit_progress(ctx, tx, query, progress_counter);
            }
            if event.get("type").and_then(Value::as_str) == Some("message_stop") {
                saw_message_stop = true;
                break;
            }
        }
        if !saw_message_stop {
            let err = HttpError::Connection("stream ended before message_stop".into());
            if acc.blocks.is_empty() {
                return Err(err);
            }
            let usage = acc.usage.clone();
            let mut blocks = acc.into_blocks();
            blocks.push(json!({ "type": "text", "text": stream_partial_notice(&err) }));
            return Ok((blocks, usage));
        }
        let usage = acc.usage.clone();
        Ok((acc.into_blocks(), usage))
    }

    /// Emit one incremental "searching" progress event, mirroring claude-code's
    /// per-search `onProgress({ type: 'query_update', query })`
    /// (`WebSearchTool.ts:344-354`). The payload carries the synthetic
    /// `search-progress-N` id and the query in `data` (TS keeps it on the event's
    /// `toolUseID`/`data`); here the channel key is the model's tool-use id when
    /// present, falling back to a fresh id (the channel key must be a real
    /// [`protocol::ToolUseId`], unlike TS's free-form string). Best-effort
    /// (`try_send`), matching TS's synchronous fire-and-forget `onProgress`.
    fn emit_progress(ctx: &ToolUseContext, tx: &ToolProgressSender, query: &str, counter: u64) {
        // `Default for ToolUseId` generates a fresh random id (same as `new()`),
        // so the channel key is the model's tool-use id when present, else fresh.
        let tool_use_id = ctx.tool_use_id.clone().unwrap_or_default();
        let _ = tx.try_send(tool_api::progress::ToolProgress {
            tool_use_id,
            data: json!({
                "type": "query_update",
                "toolUseID": format!("search-progress-{counter}"),
                "query": query,
            }),
        });
    }

    /// Build the success [`ToolCallResult`] (shared by the streaming and blocking
    /// paths) and fire `WEB_SEARCH_COMPLETED`. Output bytes are identical to the
    /// original blocking path: a `model_content` header + per-entry segments +
    /// cite-sources footer, plus the structured `results` array for the TUI.
    async fn build_success_result(
        &self,
        invocation_id: &str,
        query: &str,
        results: Vec<SearchResultEntry>,
        search_count: u64,
        input_tokens: u64,
        output_tokens: u64,
        elapsed_ms: u64,
    ) -> ToolCallResult {
        let hits = results.len() as u64;
        self.emit_completed(invocation_id, hits, input_tokens, output_tokens, elapsed_ms)
            .await;
        // Model-facing text — the `Web search results for query: "<q>"` header +
        // per-entry segments + cite-sources footer. Lives on
        // `ToolCallResult.model_content` (the dispatch uses it verbatim as the
        // tool's model text); `data` is pure metadata, 1:1 with claude-code's
        // `V7p` return `{query, results, durationSeconds, searchCount}`.
        let model_content = build_model_content(query, &results);
        // `durationSeconds = (performance.now()-s)/1000` (binary @148664): the
        // f64-seconds elapsed, not the integer millisecond `duration_ms`.
        let duration_seconds = elapsed_ms as f64 / 1000.0;
        ToolCallResult {
            data: json!({
                "query": query,
                "results": results,
                "durationSeconds": duration_seconds,
                "searchCount": search_count,
            }),
            model_content: Some(model_content),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        }
    }

    /// Map a mid-stream / connect-phase [`HttpError`] to a [`ToolError`] and emit
    /// the matching `WEB_SEARCH_FAILED` telemetry. Mirrors the blocking path's
    /// error arms so failure classification is identical across both paths.
    async fn map_stream_error(
        &self,
        invocation_id: &str,
        err: HttpError,
        elapsed_ms: u64,
    ) -> ToolError {
        match err {
            HttpError::Status { status, .. } => {
                self.emit_failed(invocation_id, "http_status", Some(status), elapsed_ms)
                    .await;
                ToolError::Transport(format!("WebSearch: HTTP {status} from messages_create"))
            }
            HttpError::Timeout(_) => {
                self.emit_failed(invocation_id, "timeout", None, elapsed_ms)
                    .await;
                ToolError::Transport("WebSearch: request timed out".into())
            }
            other => {
                self.emit_failed(invocation_id, "transport", None, elapsed_ms)
                    .await;
                ToolError::Transport(format!("WebSearch: transport failure: {other}"))
            }
        }
    }

    /// Finish the blocking fallback path: parse the buffered response via
    /// [`parse_response_content`] and build the result, or map the error. This is
    /// the original blocking `call` body, factored out so the fallback reuses it
    /// verbatim.
    async fn finish_blocking(
        &self,
        invocation_id: &str,
        query: &str,
        resp_result: Result<protocol::HttpResponse, HttpError>,
        elapsed_ms: u64,
    ) -> Result<ToolCallResult, ToolError> {
        match resp_result {
            Ok(http_resp) if http_resp.status == 200 => {
                let parsed: WebSearchMessageResponse = match serde_json::from_str(&http_resp.body) {
                    Ok(p) => p,
                    Err(e) => {
                        self.emit_failed(invocation_id, "invalid_response", None, elapsed_ms)
                            .await;
                        return Err(ToolError::Transport(format!(
                            "WebSearch: invalid response: {e}"
                        )));
                    }
                };
                let search_count = count_searches(&parsed.content);
                let results = parse_response_content(&parsed.content);
                Ok(self
                    .build_success_result(
                        invocation_id,
                        query,
                        results,
                        search_count,
                        parsed.usage.input_tokens,
                        parsed.usage.output_tokens,
                        elapsed_ms,
                    )
                    .await)
            }
            Ok(http_resp) => {
                self.emit_failed(
                    invocation_id,
                    "http_status",
                    Some(http_resp.status),
                    elapsed_ms,
                )
                .await;
                Err(ToolError::Transport(format!(
                    "WebSearch: HTTP {} from messages_create",
                    http_resp.status
                )))
            }
            Err(err) => Err(self.map_stream_error(invocation_id, err, elapsed_ms).await),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_is_verbatim_v2_1_183() {
        let d = web_search_description();
        // Opening + the CRITICAL Sources requirement (byte-exact anchors). The
        // binary's literal starts with a leading newline and a `- ` on the first
        // bullet (`\n- Allows...`) and ends with a trailing newline.
        assert!(d.starts_with(
            "\n- Allows Claude to search the web and use the results to inform responses\n"
        ));
        assert!(d.contains("CRITICAL REQUIREMENT - You MUST follow this:\n"));
        assert!(
            d.contains("  - This is MANDATORY - never skip including sources in your response\n")
        );
        assert!(d.contains("  - Example format:\n"));
        assert!(d.contains("Usage notes:\n  - Domain filtering is supported to include or block specific websites\n  - Web search is only available in the US\n"));
        // Runtime month/year slot: "<Month> <Year>" (e.g. "June 2026").
        let my = chrono::Local::now().format("%B %Y").to_string();
        assert!(d.contains(&format!(
            "The current month is {my}. You MUST use this year"
        )));
        assert!(d.ends_with("with the current year, NOT last year\n"));
    }

    #[test]
    fn concise_description_is_verbatim_cni_dh_true_branch() {
        // claude-code `CNi(model)` Dh-true branch (binary @~197074996). No
        // leading and no trailing newline; em-dash (`—`); literal backticks
        // around the domain-filter param names; `${t}` resolves to "<Month>
        // <Year>" via `U1i()` (= chrono `%B %Y`).
        let c = web_search_description_concise();
        let my = chrono::Local::now().format("%B %Y").to_string();
        assert_eq!(
            c,
            format!(
                "Search the web. Returns result blocks with titles and URLs. US-only.\n\
                 \n\
                 - The current month is {my} \u{2014} use this when searching for recent information.\n\
                 - `allowed_domains` / `blocked_domains` filter results.\n\
                 - After answering from results, end with a \"Sources:\" list of the URLs you used as markdown links."
            )
        );
        // No leading/trailing newline (distinct from the VERBOSE variant).
        assert!(
            c.starts_with("Search the web. Returns result blocks with titles and URLs. US-only.\n")
        );
        assert!(c.ends_with("as markdown links."));
        // Real em-dash byte (e2 80 94), not a hyphen.
        assert!(c.contains(" \u{2014} use this when searching"));
    }

    #[test]
    fn select_prompt_gates_concise_for_current_gen_models() {
        use std::env;
        // Neutralize any ambient env override so the model branch alone decides.
        let prev = env::var("LINGXI_SIMPLE_SYSTEM_PROMPT").ok();
        env::remove_var("LINGXI_SIMPLE_SYSTEM_PROMPT");

        // `None`/empty ⇒ Dh(undefined) ⇒ VERBOSE.
        assert_eq!(select_web_search_prompt(None), web_search_description());
        assert_eq!(select_web_search_prompt(Some("")), web_search_description());
        // Current-gen models (UWu=false ⇒ !UWu=true ⇒ Dh true) ⇒ CONCISE.
        assert_eq!(
            select_web_search_prompt(Some("claude-opus-4-8")),
            web_search_description_concise()
        );
        assert_eq!(
            select_web_search_prompt(Some("claude-fable-5-1")),
            web_search_description_concise()
        );
        assert_eq!(
            select_web_search_prompt(Some("claude-mythos-5-1")),
            web_search_description_concise()
        );
        // Classic models (UWu=true ⇒ Dh false) ⇒ VERBOSE.
        assert_eq!(
            select_web_search_prompt(Some("claude-sonnet-4-20250514")),
            web_search_description()
        );
        assert_eq!(
            select_web_search_prompt(Some("claude-opus-4-1")),
            web_search_description()
        );
        assert_eq!(
            select_web_search_prompt(Some("claude-3-5-haiku")),
            web_search_description()
        );

        // Restore the prior env state.
        match prev {
            Some(v) => env::set_var("LINGXI_SIMPLE_SYSTEM_PROMPT", v),
            None => env::remove_var("LINGXI_SIMPLE_SYSTEM_PROMPT"),
        }
    }

    #[tokio::test]
    async fn prompt_method_selects_concise_via_model_opt() {
        use std::env;
        let prev = env::var("LINGXI_SIMPLE_SYSTEM_PROMPT").ok();
        env::remove_var("LINGXI_SIMPLE_SYSTEM_PROMPT");

        // `PromptOptions::model = Some("claude-opus-4-8")` ⇒ CONCISE via the
        // tool's `prompt()` method (the `async prompt({model:e}){return CNi(e)}`
        // wiring). Default opts (model=None) ⇒ VERBOSE. `prompt()` ignores the
        // context, so any test ctx works.
        let tool = WebSearchTool::new(validation_ctx());
        let concise = tool
            .prompt(&PromptOptions {
                include_examples: false,
                model: Some("claude-opus-4-8".into()),
                model_profile: None,
            })
            .await;
        assert_eq!(concise, web_search_description_concise());
        let verbose = tool.prompt(&PromptOptions::default()).await;
        assert_eq!(verbose, web_search_description());

        match prev {
            Some(v) => env::set_var("LINGXI_SIMPLE_SYSTEM_PROMPT", v),
            None => env::remove_var("LINGXI_SIMPLE_SYSTEM_PROMPT"),
        }
    }

    #[test]
    fn locked_constants_match_spec() {
        assert_eq!(WEB_SEARCH_TOOL_BLOCK_TYPE, "web_search_20250305");
        assert_eq!(WEB_SEARCH_TOOL_BLOCK_NAME, "web_search");
        assert_eq!(WEB_SEARCH_MAX_USES, 8);
        assert_eq!(WEB_SEARCH_DEFAULT_MAX_TOKENS, 4096);
        assert_eq!(TOOL_NAME, "WebSearch");
    }

    #[test]
    fn tool_max_result_size_matches_claude_limit() {
        let tool = WebSearchTool::new(validation_ctx());
        assert_eq!(tool.max_result_size_chars(), 100_000);
    }

    #[test]
    fn anthropic_beta_lock() {
        assert_eq!(WEB_SEARCH_BETA, "web-search-2025-03-05");
    }

    // ---- isEnabled provider gating (WebSearchTool.ts:168-193) ---------------

    #[test]
    fn is_enabled_first_party_any_model() {
        assert!(web_search_is_enabled(
            ApiProvider::FirstParty,
            "claude-sonnet-4-20250514"
        ));
        assert!(web_search_is_enabled(
            ApiProvider::FirstParty,
            "claude-3-5-haiku"
        ));
        assert!(web_search_is_enabled(
            ApiProvider::FirstParty,
            "literally-anything"
        ));
    }

    #[test]
    fn is_enabled_vertex_only_claude_4x() {
        // `claude-fable-5` is the first disjunct upstream (binary @202215129).
        assert!(web_search_is_enabled(
            ApiProvider::Vertex,
            "claude-fable-5-1"
        ));
        assert!(web_search_is_enabled(
            ApiProvider::Vertex,
            "claude-fable-5-1-20260901"
        ));
        assert!(web_search_is_enabled(
            ApiProvider::Vertex,
            "claude-opus-4-20250514"
        ));
        assert!(web_search_is_enabled(
            ApiProvider::Vertex,
            "claude-sonnet-4-5"
        ));
        assert!(web_search_is_enabled(
            ApiProvider::Vertex,
            "claude-haiku-4-5"
        ));
        // Pre-4.x and non-Claude models on Vertex are disabled.
        assert!(!web_search_is_enabled(
            ApiProvider::Vertex,
            "claude-3-5-sonnet"
        ));
        assert!(!web_search_is_enabled(
            ApiProvider::Vertex,
            "gemini-2.5-pro"
        ));
    }

    #[test]
    fn is_enabled_foundry_any_model() {
        assert!(web_search_is_enabled(ApiProvider::Foundry, "anything"));
    }

    #[test]
    fn is_enabled_other_provider_disabled() {
        assert!(!web_search_is_enabled(
            ApiProvider::Other,
            "claude-opus-4-20250514"
        ));
        assert!(!web_search_is_enabled(ApiProvider::Other, "anything"));
    }

    #[test]
    fn hosted_gate_none_profile_requires_a_claude_live_model() {
        // Explicit anthropic profile → hosted regardless of model.
        assert!(hosted_web_search_enabled(
            Some("anthropic"),
            ApiProvider::FirstParty,
            "claude-opus-4-8"
        ));
        // Any other explicit profile → client-side.
        assert!(!hosted_web_search_enabled(
            Some("deepseek"),
            ApiProvider::FirstParty,
            "deepseek-v4-pro"
        ));
        // No profile + Claude live model on a hosted provider → hosted
        // (claude-code parity: its `None` path always had a Claude model).
        assert!(hosted_web_search_enabled(
            None,
            ApiProvider::FirstParty,
            "claude-sonnet-5"
        ));
        // No profile + NON-Claude live model → client-side, EVEN THOUGH the
        // fallback provider is FirstParty (the boot base_url). This is the
        // regression fix: a resumed cross-provider (profile=None) deepseek turn
        // must NOT get the hosted framing/path.
        assert!(!hosted_web_search_enabled(
            None,
            ApiProvider::FirstParty,
            "deepseek-v4-pro"
        ));
        assert!(!hosted_web_search_enabled(
            None,
            ApiProvider::FirstParty,
            "openrouter/auto"
        ));
    }

    #[test]
    fn infer_api_provider_from_base_url() {
        assert_eq!(
            infer_api_provider("https://api.anthropic.com"),
            ApiProvider::FirstParty
        );
        assert_eq!(
            infer_api_provider("https://us-central1-aiplatform.googleapis.com"),
            ApiProvider::Vertex
        );
        assert_eq!(
            infer_api_provider("https://aiplatform.googleapis.com/v1"),
            ApiProvider::Vertex
        );
        assert_eq!(
            infer_api_provider("https://my-resource.openai.azure.com"),
            ApiProvider::Foundry
        );
        assert_eq!(
            infer_api_provider("https://bedrock-runtime.us-east-1.amazonaws.com"),
            ApiProvider::Other
        );
        // Unparseable base_url => Other (disabled).
        assert_eq!(infer_api_provider("not a url"), ApiProvider::Other);
    }

    #[test]
    fn builds_correct_tool_block() {
        let input = WebSearchInput {
            query: "rust async traits".into(),
            allowed_domains: None,
            blocked_domains: None,
        };
        let block = build_tool_block(&input);
        assert_eq!(block["type"], "web_search_20250305");
        assert_eq!(block["name"], "web_search");
        assert_eq!(block["max_uses"], 8);
        assert!(block.get("allowed_domains").is_none());
        assert!(block.get("blocked_domains").is_none());
    }

    #[test]
    fn tool_block_includes_allowed_domains_when_set() {
        let input = WebSearchInput {
            query: "x".into(),
            allowed_domains: Some(vec!["docs.rs".into(), "crates.io".into()]),
            blocked_domains: None,
        };
        let block = build_tool_block(&input);
        let allowed = block["allowed_domains"].as_array().expect("array");
        assert_eq!(allowed.len(), 2);
        assert_eq!(allowed[0], "docs.rs");
        assert_eq!(allowed[1], "crates.io");
    }

    #[test]
    fn tool_block_includes_blocked_domains_when_set() {
        let input = WebSearchInput {
            query: "x".into(),
            allowed_domains: None,
            blocked_domains: Some(vec!["spam.example".into()]),
        };
        let block = build_tool_block(&input);
        assert_eq!(block["blocked_domains"][0], "spam.example");
    }

    #[test]
    fn body_has_model_and_message() {
        let input = WebSearchInput {
            query: "rust async traits".into(),
            allowed_domains: None,
            blocked_domains: None,
        };
        let body = build_request_body("claude-sonnet-4-20250514", &input);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["messages"][0]["role"], "user");
        // claude-code prefixes the query (WebSearchTool.ts:258) + sets a system
        // prompt (asSystemPrompt(['You are an assistant for performing a web
        // search tool use']), WebSearchTool.ts:270-271).
        assert_eq!(
            body["messages"][0]["content"],
            "Perform a web search for the query: rust async traits"
        );
        assert_eq!(
            body["system"],
            "You are an assistant for performing a web search tool use"
        );
        assert_eq!(body["tools"][0]["type"], "web_search_20250305");
    }

    #[test]
    fn consecutive_text_blocks_concatenate_into_one_entry() {
        // Mirrors upstream: while `in_text`, consecutive `text` blocks append to
        // the same buffer and flush as a SINGLE entry at the end (not one per
        // block).
        let blocks = vec![
            json!({ "type": "text", "text": "Here are some results:" }),
            json!({ "type": "text", "text": "1. ..." }),
        ];
        let parsed = parse_response_content(&blocks);
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            SearchResultEntry::Text(s) => assert_eq!(s, "Here are some results:1. ..."),
            SearchResultEntry::Hit(_) => panic!("expected Text"),
        }
    }

    #[test]
    fn web_search_tool_result_success_produces_hit() {
        // The QUERY-carrying `server_tool_use` block is NOT a result; the actual
        // results live in the `web_search_tool_result` block's `content` array.
        let blocks = vec![
            json!({
                "type": "server_tool_use",
                "id": "stu_1",
                "name": "web_search",
                "input": { "query": "rust async" }
            }),
            json!({
                "type": "web_search_tool_result",
                "tool_use_id": "stu_1",
                "content": [
                    { "title": "Docs.rs", "url": "https://docs.rs", "encrypted_content": "zzz" },
                    { "title": "crates.io", "url": "https://crates.io" }
                ]
            }),
        ];
        let parsed = parse_response_content(&blocks);
        assert_eq!(parsed.len(), 1, "server_tool_use must not produce a result");
        match &parsed[0] {
            SearchResultEntry::Hit(v) => {
                assert_eq!(v["tool_use_id"], "stu_1");
                let hits = v["content"].as_array().expect("content array");
                assert_eq!(hits.len(), 2);
                // Only `title` and `url` are projected (extra fields dropped).
                assert_eq!(
                    hits[0],
                    json!({ "title": "Docs.rs", "url": "https://docs.rs" })
                );
                assert_eq!(
                    hits[1],
                    json!({ "title": "crates.io", "url": "https://crates.io" })
                );
            }
            SearchResultEntry::Text(_) => panic!("expected Hit"),
        }
    }

    #[test]
    fn web_search_tool_result_error_produces_error_string() {
        // When `content` is an error object (not an array), upstream pushes the
        // `Web search error: <error_code>` string.
        let blocks = vec![json!({
            "type": "web_search_tool_result",
            "tool_use_id": "stu_9",
            "content": { "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded" }
        })];
        let parsed = parse_response_content(&blocks);
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            SearchResultEntry::Text(s) => assert_eq!(s, "Web search error: max_uses_exceeded"),
            SearchResultEntry::Hit(_) => panic!("expected Text error string"),
        }
    }

    #[test]
    fn server_tool_use_flushes_accumulated_text() {
        // Leading text accumulates, then a `server_tool_use` flushes it (trimmed)
        // as one entry; the `server_tool_use` itself is never emitted.
        let blocks = vec![
            json!({ "type": "text", "text": "Found: " }),
            json!({ "type": "text", "text": "  things  " }),
            json!({
                "type": "server_tool_use",
                "id": "stu_1",
                "name": "web_search",
                "input": { "query": "q" }
            }),
        ];
        let parsed = parse_response_content(&blocks);
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            SearchResultEntry::Text(s) => assert_eq!(s, "Found:   things"),
            SearchResultEntry::Hit(_) => panic!("expected Text"),
        }
    }

    #[test]
    fn canonical_sequence_text_tooluse_result_text() {
        // The documented per-search block order, with a trailing commentary text
        // block after the result.
        let blocks = vec![
            json!({ "type": "text", "text": "Let me search." }),
            json!({
                "type": "server_tool_use",
                "id": "stu_1",
                "name": "web_search",
                "input": { "query": "q" }
            }),
            json!({
                "type": "web_search_tool_result",
                "tool_use_id": "stu_1",
                "content": [ { "title": "T", "url": "https://t.example" } ]
            }),
            json!({ "type": "text", "text": "Here is what I found." }),
        ];
        let parsed = parse_response_content(&blocks);
        assert_eq!(parsed.len(), 3);
        assert!(matches!(&parsed[0], SearchResultEntry::Text(s) if s == "Let me search."));
        assert!(matches!(&parsed[1], SearchResultEntry::Hit(_)));
        assert!(matches!(&parsed[2], SearchResultEntry::Text(s) if s == "Here is what I found."));
    }

    #[test]
    fn server_tool_use_alone_yields_no_results() {
        // A lone `server_tool_use` (query carrier) with no result block produces
        // nothing — it only flushes the (empty) text buffer.
        let blocks = vec![json!({
            "type": "server_tool_use",
            "id": "stu_2",
            "name": "advisor",
            "input": {}
        })];
        let parsed = parse_response_content(&blocks);
        assert!(
            parsed.is_empty(),
            "server_tool_use must not become a result"
        );
    }

    #[test]
    fn model_content_has_header_and_reminder_footer() {
        let results = vec![
            SearchResultEntry::Text("First summary.".into()),
            SearchResultEntry::Hit(json!({
                "content": [ { "title": "Docs.rs", "url": "https://docs.rs" } ]
            })),
            SearchResultEntry::Hit(json!({ "query": "rust async" })),
        ];
        let mc = build_model_content("rust async", &results);
        // Exact TS header bytes.
        assert!(
            mc.starts_with("Web search results for query: \"rust async\"\n\n"),
            "model_content must start with the TS header, got: {mc}"
        );
        // Text entry rendered verbatim.
        assert!(mc.contains("First summary."));
        // Hit with a non-empty `content` array renders a `Links:` JSON line.
        assert!(
            mc.contains("Links: [{\"title\":\"Docs.rs\",\"url\":\"https://docs.rs\"}]"),
            "hit with content must render Links: <json>, got: {mc}"
        );
        // Hit without a `content` array renders the `No links found.` fallback.
        assert!(mc.contains("No links found."));
        // Exact TS footer bytes, and trailing `.trim()` means it ends there.
        assert!(
            mc.ends_with(
                "REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."
            ),
            "model_content must end with the REMINDER footer, got: {mc}"
        );
    }

    #[test]
    fn model_content_trims_and_keeps_footer_when_no_results() {
        let mc = build_model_content("q", &[]);
        assert!(mc.starts_with("Web search results for query: \"q\""));
        assert!(mc.ends_with(
            "REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."
        ));
        // `.trim()` removes any trailing whitespace; no trailing newline.
        assert_eq!(mc, mc.trim());
    }

    #[test]
    fn count_searches_is_max_of_server_tool_use_and_result_blocks() {
        // 1:1 with `V7p`: searchCount = Math.max(i, a) where i = #server_tool_use,
        // a = #web_search_tool_result.
        // Balanced single search → max(1,1) = 1.
        let one = vec![
            json!({ "type": "server_tool_use", "id": "s", "name": "web_search", "input": {} }),
            json!({ "type": "web_search_tool_result", "tool_use_id": "s", "content": [] }),
        ];
        assert_eq!(count_searches(&one), 1);
        // Two searches → max(2,2) = 2; interleaved text blocks are ignored.
        let two = vec![
            json!({ "type": "text", "text": "x" }),
            json!({ "type": "server_tool_use", "id": "s1", "name": "web_search", "input": {} }),
            json!({ "type": "web_search_tool_result", "tool_use_id": "s1", "content": [] }),
            json!({ "type": "server_tool_use", "id": "s2", "name": "web_search", "input": {} }),
            json!({ "type": "web_search_tool_result", "tool_use_id": "s2", "content": [] }),
        ];
        assert_eq!(count_searches(&two), 2);
        // Unbalanced (a result block with no matching server_tool_use) → max(1,2).
        let unbalanced = vec![
            json!({ "type": "server_tool_use", "id": "s1", "name": "web_search", "input": {} }),
            json!({ "type": "web_search_tool_result", "tool_use_id": "s1", "content": [] }),
            json!({ "type": "web_search_tool_result", "tool_use_id": "s2", "content": [] }),
        ];
        assert_eq!(count_searches(&unbalanced), 2);
        // No search blocks at all → 0.
        assert_eq!(count_searches(&[json!({ "type": "text", "text": "x" })]), 0);
    }

    #[test]
    fn ignores_thinking_blocks() {
        let blocks = vec![json!({ "type": "thinking", "thinking": "let me think" })];
        let parsed = parse_response_content(&blocks);
        assert!(parsed.is_empty());
    }

    // ---- async impl Tool tests ---------------------------------------------
    //
    // WebSearch now streams the `/v1/messages` request (`stream_sse`) and
    // reassembles the content blocks incrementally; the blocking `request` path
    // survives only as the connect-failure fallback. The HTTP-exercising tests
    // below therefore drive the STREAMING path via a scripted SSE event
    // sequence, except `fallback_*` which force a `stream_sse` connect error and
    // assert the blocking fallback.

    use platform_api::http::HttpTransport;
    use std::sync::Arc;
    use telemetry::sinks::InMemorySink;
    use telemetry::AnalyticsBus;
    use tool_api::progress::{progress_channel, ToolProgress, ToolProgressReceiver};
    use tool_api::test_support::fresh_ctx;

    /// A streaming-aware mock `HttpTransport` with INDEPENDENT queues for
    /// `stream_sse` (SSE event frames) and `request` (blocking responses), so a
    /// single test can script the streaming path AND a distinct blocking
    /// fallback. `fail_stream` forces `stream_sse` to error at connect (without
    /// touching the blocking queue), exercising the fallback path. Every
    /// received request (stream or blocking) is recorded for header/body asserts.
    #[derive(Default)]
    struct StreamingMockHttp {
        sse_events: std::sync::Mutex<Option<Vec<protocol::SseEvent>>>,
        blocking:
            std::sync::Mutex<std::collections::VecDeque<Result<protocol::HttpResponse, HttpError>>>,
        fail_stream: std::sync::atomic::AtomicBool,
        received: std::sync::Mutex<Vec<protocol::HttpRequest>>,
    }

    impl StreamingMockHttp {
        fn new() -> Self {
            Self::default()
        }
        /// Script the SSE frames `stream_sse` will yield (one per poll).
        fn set_stream(&self, events: Vec<protocol::SseEvent>) {
            *self.sse_events.lock().unwrap() = Some(events);
        }
        /// Force `stream_sse` to return a connect error (drives the fallback).
        fn fail_stream(&self) {
            self.fail_stream
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        /// Enqueue a blocking `request` response (used by the fallback path).
        fn enqueue_blocking(&self, resp: Result<protocol::HttpResponse, HttpError>) {
            self.blocking.lock().unwrap().push_back(resp);
        }
        fn received_requests(&self) -> Vec<protocol::HttpRequest> {
            self.received.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HttpTransport for StreamingMockHttp {
        async fn request(
            &self,
            req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, HttpError> {
            self.received.lock().unwrap().push(req);
            self.blocking
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Err(HttpError::InvalidResponse(
                        "StreamingMockHttp: no blocking response scripted".into(),
                    ))
                })
        }
        async fn stream_sse(
            &self,
            req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, HttpError> {
            self.received.lock().unwrap().push(req);
            if self.fail_stream.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(HttpError::InvalidRequest(
                    "StreamingMockHttp: stream_sse forced failure".into(),
                ));
            }
            let events = self.sse_events.lock().unwrap().take().unwrap_or_default();
            Ok(Box::pin(VecSseStream {
                remaining: events.into(),
            }))
        }
    }

    /// Minimal `Stream` yielding the scripted SSE frames one per poll, then end.
    struct VecSseStream {
        remaining: std::collections::VecDeque<protocol::SseEvent>,
    }
    impl futures_util::Stream for VecSseStream {
        type Item = Result<protocol::SseEvent, HttpError>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(self.remaining.pop_front().map(Ok))
        }
    }

    /// One SSE frame whose `data` is the JSON string of `event`.
    fn sse(event: Value) -> protocol::SseEvent {
        protocol::SseEvent {
            event_type: event
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string),
            data: event.to_string(),
            id: None,
        }
    }

    /// Build a ctx wired to `http`, returning the ctx + sink for event asserts.
    /// A call-context representing an ANTHROPIC session, for the hosted
    /// web-search path tests. `fresh_ctx`'s placeholder `main_loop_model`
    /// ("test") now routes CLIENT-side under the live-model gate, so hosted
    /// tests pin the profile explicitly (`Some("anthropic")` ⇒ hosted).
    fn anthropic_ctx() -> tool_api::ToolUseContext {
        let mut c = fresh_ctx();
        c.options.model_profile = Some("anthropic".to_string());
        c
    }

    fn make_streaming_ctx(http: Arc<StreamingMockHttp>) -> (BuiltinToolContext, Arc<InMemorySink>) {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.http = http as Arc<dyn HttpTransport>;
        ctx.provider = Arc::new(tool_api::AnthropicRequestBuilder::new("test-key", None));
        ctx.default_model = "claude-sonnet-4-20250514".into();
        (ctx, sink)
    }

    #[derive(Clone)]
    struct StaticWebSearchConfig {
        cfg: platform_api::WebSearchRuntimeConfig,
    }

    #[async_trait]
    impl platform_api::WebSearchConfigProvider for StaticWebSearchConfig {
        async fn load_web_search_config(&self) -> platform_api::WebSearchRuntimeConfig {
            self.cfg.clone()
        }
    }

    /// A blocking-200 response wrapping `body`. Returns the `Result` shape that
    /// `enqueue_blocking` accepts (so a test may also enqueue an `Err`).
    #[allow(clippy::unnecessary_wraps)]
    fn ok_blocking(status: u16, body: &str) -> Result<protocol::HttpResponse, HttpError> {
        Ok(protocol::HttpResponse {
            status,
            headers: vec![],
            body: body.to_string(),
            body_bytes: Vec::new(),
        })
    }

    #[tokio::test]
    async fn client_side_auto_falls_back_after_bad_tavily_key() {
        let http = Arc::new(StreamingMockHttp::new());
        http.enqueue_blocking(ok_blocking(
            401,
            r#"{"detail":{"error":"Unauthorized: missing or invalid API key."}}"#,
        ));
        http.enqueue_blocking(ok_blocking(
            200,
            r#"<a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fweather" class="result-link">Weather</a><td class="result-snippet">Current forecast</td>"#,
        ));
        let (mut ctx, _sink) = make_streaming_ctx(http.clone());
        ctx.provider = Arc::new(tool_api::AnthropicRequestBuilder::new(
            "test-key",
            Some("https://api.openai.example".into()),
        ));
        ctx.web_search_config = Some(Arc::new(StaticWebSearchConfig {
            cfg: platform_api::WebSearchRuntimeConfig {
                provider: Some("auto".into()),
                searxng_url: None,
                tavily_key: Some("bad-key".into()),
                brave_key: None,
            },
        }));
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let res = tool
            .call(json!({ "query": "wuhan weather" }), fresh_ctx(), tx)
            .await
            .expect("auto fallback should succeed");

        assert_eq!(res.data["provider"], "DuckDuckGo");
        let mc = res.model_content.as_deref().expect("model content");
        assert!(mc.contains("[Weather](https://example.com/weather)"));
        let reqs = http.received_requests();
        assert_eq!(reqs.len(), 2, "Tavily failure then DuckDuckGo fallback");
        assert!(reqs[0].url.contains("api.tavily.com/search"));
        assert!(reqs[1].url.contains("lite.duckduckgo.com/lite/"));
    }

    /// Drain a progress receiver synchronously (the channel is unbuffered-ish
    /// 64-slot; `try_recv` until empty).
    fn drain_progress(rx: &mut ToolProgressReceiver) -> Vec<ToolProgress> {
        let mut out = Vec::new();
        while let Ok(p) = rx.try_recv() {
            out.push(p);
        }
        out
    }

    #[tokio::test]
    async fn streaming_path_reassembles_and_emits_completed() {
        // Scripted Anthropic message-stream for ONE search:
        //   message_start (usage) ; text block "Here are results:" via deltas ;
        //   server_tool_use block (query via input_json_delta) ;
        //   web_search_tool_result block with a 2-result `content` array ;
        //   trailing commentary text ; message_delta (output usage) ; message_stop.
        let http = Arc::new(StreamingMockHttp::new());
        http.set_stream(vec![
            sse(json!({ "type": "message_start", "message": { "usage": { "input_tokens": 42, "output_tokens": 0 } } })),
            // Block 0: leading text, delivered in two text_delta chunks.
            sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Here are " } })),
            sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "results:" } })),
            sse(json!({ "type": "content_block_stop", "index": 0 })),
            // Block 1: server_tool_use; the query arrives via input_json_delta.
            sse(json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "server_tool_use", "id": "stu_1", "name": "web_search", "input": {} } })),
            sse(json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"query\":\"rust " } })),
            sse(json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "async\"}" } })),
            sse(json!({ "type": "content_block_stop", "index": 1 })),
            // Block 2: web_search_tool_result carrying a 2-hit content array.
            sse(json!({ "type": "content_block_start", "index": 2, "content_block": {
                "type": "web_search_tool_result", "tool_use_id": "stu_1",
                "content": [
                    { "title": "docs.rs", "url": "https://docs.rs", "encrypted_content": "zzz" },
                    { "title": "crates.io", "url": "https://crates.io" }
                ]
            } })),
            sse(json!({ "type": "content_block_stop", "index": 2 })),
            // Block 3: trailing commentary text + a citation delta.
            sse(json!({ "type": "content_block_start", "index": 3, "content_block": { "type": "text", "text": "" } })),
            sse(json!({ "type": "content_block_delta", "index": 3, "delta": { "type": "text_delta", "text": "Done." } })),
            sse(json!({ "type": "content_block_delta", "index": 3, "delta": { "type": "citations_delta", "citation": { "url": "https://docs.rs" } } })),
            sse(json!({ "type": "content_block_stop", "index": 3 })),
            sse(json!({ "type": "message_delta", "usage": { "output_tokens": 17 } })),
            sse(json!({ "type": "message_stop" })),
        ]);
        let (ctx, sink) = make_streaming_ctx(http);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("ok");

        // Reassembled blocks → parse_response_content yields: leading text entry,
        // one structured hit (server_tool_use dropped), trailing text entry.
        let arr = res.data["results"].as_array().expect("results array");
        assert_eq!(arr.len(), 3, "got: {arr:?}");
        assert_eq!(arr[0], "Here are results:");
        assert_eq!(arr[1]["tool_use_id"], "stu_1");
        let hits = arr[1]["content"].as_array().expect("hits");
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            json!({ "title": "docs.rs", "url": "https://docs.rs" })
        );
        assert_eq!(
            hits[1],
            json!({ "title": "crates.io", "url": "https://crates.io" })
        );
        assert_eq!(arr[2], "Done.");

        // `data` is pure metadata (1:1 with `V7p`): the camelCase
        // `durationSeconds` (f64 seconds) + `searchCount` (= max(server_tool_use,
        // web_search_tool_result) = max(1,1) = 1 here). No `model_content` /
        // `duration_ms` keys remain in `data`.
        assert!(res.data.get("model_content").is_none());
        assert!(res.data.get("duration_ms").is_none());
        assert!(res.data.get("durationSeconds").is_some());
        assert_eq!(res.data["searchCount"], 1);

        // Final formatted output matches the blocking path for the equivalent
        // full response (same header / Links: / footer bytes). The model-facing
        // text now lives on `ToolCallResult.model_content`, NOT in `data`.
        let mc = res.model_content.as_deref().expect("model_content");
        let equivalent_blocks = vec![
            json!({ "type": "text", "text": "Here are results:" }),
            json!({ "type": "server_tool_use", "id": "stu_1", "name": "web_search", "input": { "query": "rust async" } }),
            json!({ "type": "web_search_tool_result", "tool_use_id": "stu_1", "content": [
                { "title": "docs.rs", "url": "https://docs.rs" },
                { "title": "crates.io", "url": "https://crates.io" }
            ] }),
            json!({ "type": "text", "text": "Done." }),
        ];
        let expected_mc =
            build_model_content("rust async", &parse_response_content(&equivalent_blocks));
        assert_eq!(
            mc, expected_mc,
            "streamed output must equal blocking output"
        );

        // COMPLETED telemetry fired, with usage captured from the stream.
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_search_completed"));
    }

    #[tokio::test]
    async fn streaming_path_emits_progress_on_server_tool_use_start() {
        // A server_tool_use block START must yield at least one progress event,
        // mirroring TS's per-search `onProgress`.
        let http = Arc::new(StreamingMockHttp::new());
        http.set_stream(vec![
            sse(json!({ "type": "message_start", "message": { "usage": { "input_tokens": 1, "output_tokens": 0 } } })),
            sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "server_tool_use", "id": "stu_1", "name": "web_search", "input": {} } })),
            sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"query\":\"q\"}" } })),
            sse(json!({ "type": "content_block_stop", "index": 0 })),
            sse(json!({ "type": "content_block_start", "index": 1, "content_block": {
                "type": "web_search_tool_result", "tool_use_id": "stu_1", "content": []
            } })),
            sse(json!({ "type": "content_block_stop", "index": 1 })),
            sse(json!({ "type": "message_stop" })),
        ]);
        let (ctx, _sink) = make_streaming_ctx(http);
        let tool = WebSearchTool::new(ctx);
        let (tx, mut rx) = progress_channel();
        let _res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("ok");
        let progress = drain_progress(&mut rx);
        assert!(
            !progress.is_empty(),
            "at least one progress event must be emitted on server_tool_use start"
        );
        // Payload mirrors TS's progress shape.
        let p0 = &progress[0];
        assert_eq!(p0.data["type"], "query_update");
        assert_eq!(p0.data["query"], "rust async");
        assert!(p0.data["toolUseID"]
            .as_str()
            .is_some_and(|s| s.starts_with("search-progress-")));
    }

    #[tokio::test]
    async fn fallback_to_blocking_when_stream_sse_errs() {
        // `stream_sse` errors at connect → the tool falls back to the blocking
        // `request` path and still returns results.
        let http = Arc::new(StreamingMockHttp::new());
        http.fail_stream();
        let resp_body = json!({
            "id": "msg_fb",
            "model": "claude-sonnet-4-20250514",
            "content": [
                { "type": "text", "text": "Here are results:" },
                { "type": "server_tool_use", "id": "stu_1", "name": "web_search", "input": { "query": "rust async" } },
                { "type": "web_search_tool_result", "tool_use_id": "stu_1", "content": [ { "title": "docs.rs", "url": "https://docs.rs" } ] }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 42, "output_tokens": 17 }
        });
        http.enqueue_blocking(ok_blocking(200, &resp_body.to_string()));
        let (ctx, sink) = make_streaming_ctx(http.clone());
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("fallback must succeed");
        let arr = res.data["results"].as_array().expect("results array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "Here are results:");
        assert_eq!(arr[1]["content"][0]["url"], "https://docs.rs");
        // Exactly two requests recorded: the failed stream_sse + the fallback.
        assert_eq!(http.received_requests().len(), 2);
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_search_completed"));
    }

    #[tokio::test]
    async fn streaming_attaches_anthropic_beta_header_and_stream_flag() {
        let http = Arc::new(StreamingMockHttp::new());
        http.set_stream(vec![sse(json!({ "type": "message_stop" }))]);
        let (ctx, _sink) = make_streaming_ctx(http.clone());
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let _ = tool
            .call(json!({ "query": "foo" }), anthropic_ctx(), tx)
            .await
            .expect("ok");
        let reqs = http.received_requests();
        let last_req = reqs.last().expect("captured");
        let (_, beta) = last_req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .expect("anthropic-beta header present");
        assert!(
            beta.contains("web-search-2025-03-05"),
            "anthropic-beta `{beta}` must contain web-search-2025-03-05"
        );
        // Streaming body must carry `"stream": true` + the locked tool block.
        let body: Value =
            serde_json::from_str(last_req.body.as_ref().expect("has body")).expect("json");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tools"][0]["type"], "web_search_20250305");
        assert_eq!(body["tools"][0]["name"], "web_search");
        assert_eq!(body["tools"][0]["max_uses"], 8);
    }

    #[tokio::test]
    async fn streaming_surfaces_http_500_via_fallback_as_transport() {
        // Connect-phase stream failure → fallback `request` returns 500 → mapped
        // to Transport + FAILED telemetry.
        let http = Arc::new(StreamingMockHttp::new());
        http.fail_stream();
        http.enqueue_blocking(ok_blocking(500, "boom"));
        let (ctx, sink) = make_streaming_ctx(http);
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let err = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect_err("500 must be Err");
        assert!(matches!(err, ToolError::Transport(_)));
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_search_failed"));
    }

    #[tokio::test]
    async fn mid_stream_transport_error_is_surfaced() {
        // An error item mid-stream aborts the search as Transport (no fallback —
        // the stream was already open). Build a stream that yields one good frame
        // then an Err.
        struct ErrAfterOne;
        #[async_trait]
        impl HttpTransport for ErrAfterOne {
            async fn request(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, HttpError> {
                Err(HttpError::InvalidRequest("not used".into()))
            }
            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, HttpError> {
                struct S(u8);
                impl futures_util::Stream for S {
                    type Item = Result<protocol::SseEvent, HttpError>;
                    fn poll_next(
                        mut self: std::pin::Pin<&mut Self>,
                        _cx: &mut std::task::Context<'_>,
                    ) -> std::task::Poll<Option<Self::Item>> {
                        self.0 += 1;
                        match self.0 {
                            1 => std::task::Poll::Ready(Some(Ok(protocol::SseEvent {
                                event_type: Some("message_start".into()),
                                data:
                                    json!({ "type": "message_start", "message": { "usage": {} } })
                                        .to_string(),
                                id: None,
                            }))),
                            2 => std::task::Poll::Ready(Some(Err(HttpError::Connection(
                                "mid-stream drop".into(),
                            )))),
                            _ => std::task::Poll::Ready(None),
                        }
                    }
                }
                Ok(Box::pin(S(0)))
            }
        }
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.http = Arc::new(ErrAfterOne) as Arc<dyn HttpTransport>;
        ctx.provider = Arc::new(tool_api::AnthropicRequestBuilder::new("test-key", None));
        ctx.default_model = "claude-sonnet-4-20250514".into();
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let err = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect_err("mid-stream error must surface");
        assert!(matches!(err, ToolError::Transport(_)));
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_search_failed"));
    }

    #[tokio::test]
    async fn mid_stream_error_after_a_result_block_salvages_the_partial() {
        // CC 2.1.207: a mid-stream transport error AFTER a `web_search_tool_result`
        // block was received keeps the received result (plus an incomplete-response
        // notice) as a SUCCESS — not a hard tool error. Stream: message_start →
        // the result block → an Err.
        struct ResultThenErr;
        #[async_trait]
        impl HttpTransport for ResultThenErr {
            async fn request(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, HttpError> {
                Err(HttpError::InvalidRequest("not used".into()))
            }
            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, HttpError> {
                struct S(u8);
                impl futures_util::Stream for S {
                    type Item = Result<protocol::SseEvent, HttpError>;
                    fn poll_next(
                        mut self: std::pin::Pin<&mut Self>,
                        _cx: &mut std::task::Context<'_>,
                    ) -> std::task::Poll<Option<Self::Item>> {
                        self.0 += 1;
                        match self.0 {
                            1 => std::task::Poll::Ready(Some(Ok(protocol::SseEvent {
                                event_type: Some("message_start".into()),
                                data: json!({ "type": "message_start", "message": { "usage": { "input_tokens": 5 } } }).to_string(),
                                id: None,
                            }))),
                            2 => std::task::Poll::Ready(Some(Ok(protocol::SseEvent {
                                event_type: Some("content_block_start".into()),
                                data: json!({
                                    "type": "content_block_start",
                                    "index": 0,
                                    "content_block": {
                                        "type": "web_search_tool_result",
                                        "tool_use_id": "stu_1",
                                        "content": [ { "title": "Docs.rs", "url": "https://docs.rs" } ]
                                    }
                                })
                                .to_string(),
                                id: None,
                            }))),
                            3 => std::task::Poll::Ready(Some(Err(HttpError::Connection(
                                "mid-stream drop".into(),
                            )))),
                            _ => std::task::Poll::Ready(None),
                        }
                    }
                }
                Ok(Box::pin(S(0)))
            }
        }
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.http = Arc::new(ResultThenErr) as Arc<dyn HttpTransport>;
        ctx.provider = Arc::new(tool_api::AnthropicRequestBuilder::new("test-key", None));
        ctx.default_model = "claude-sonnet-4-20250514".into();
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("received results must be salvaged as a success, not a hard error");
        // Not an error result.
        assert!(
            !res.is_error,
            "salvaged partial must not be an error result"
        );
        // The received hit survived into the structured results.
        let results = res.data["results"].as_array().expect("results array");
        assert!(
            results
                .iter()
                .any(|r| r
                    .get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|hits| hits
                        .iter()
                        .any(|h| h.get("url").and_then(Value::as_str) == Some("https://docs.rs")))),
            "the received search result must survive: {results:?}"
        );
        // The incomplete-response notice is appended to the model-facing text.
        let mc = res.model_content.as_deref().expect("model_content");
        assert!(
            mc.contains(
                "API Error: Connection lost mid-response. The response above may be incomplete."
            ),
            "the incomplete-response notice must be present: {mc}"
        );
        // COMPLETED (not FAILED) telemetry fired.
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_search_completed"));
        assert!(!names.contains(&"tengu_tool_web_search_failed"));
    }

    #[tokio::test]
    async fn eof_without_message_stop_after_result_block_marks_partial() {
        struct ResultThenEof;
        #[async_trait]
        impl HttpTransport for ResultThenEof {
            async fn request(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, HttpError> {
                Err(HttpError::InvalidRequest("not used".into()))
            }
            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, HttpError> {
                struct S(u8);
                impl futures_util::Stream for S {
                    type Item = Result<protocol::SseEvent, HttpError>;
                    fn poll_next(
                        mut self: std::pin::Pin<&mut Self>,
                        _cx: &mut std::task::Context<'_>,
                    ) -> std::task::Poll<Option<Self::Item>> {
                        self.0 += 1;
                        match self.0 {
                            1 => std::task::Poll::Ready(Some(Ok(protocol::SseEvent {
                                event_type: Some("content_block_start".into()),
                                data: json!({
                                    "type": "content_block_start",
                                    "index": 0,
                                    "content_block": {
                                        "type": "web_search_tool_result",
                                        "tool_use_id": "stu_1",
                                        "content": [ { "title": "Docs.rs", "url": "https://docs.rs" } ]
                                    }
                                })
                                .to_string(),
                                id: None,
                            }))),
                            _ => std::task::Poll::Ready(None),
                        }
                    }
                }
                Ok(Box::pin(S(0)))
            }
        }

        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.http = Arc::new(ResultThenEof) as Arc<dyn HttpTransport>;
        ctx.provider = Arc::new(tool_api::AnthropicRequestBuilder::new("test-key", None));
        ctx.default_model = "claude-sonnet-4-20250514".into();
        ctx.bus.attach_sink(sink).await;
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("received results must be salvaged when EOF is incomplete");
        assert!(!res.is_error);
        let mc = res.model_content.as_deref().expect("model_content");
        assert!(
            mc.contains(
                "API Error: Connection lost mid-response. The response above may be incomplete."
            ),
            "EOF without message_stop must append incomplete notice: {mc}"
        );
    }

    /// Minimal ctx for tests that exercise `validate_input` only (no HTTP).
    fn validation_ctx() -> BuiltinToolContext {
        let http = Arc::new(StreamingMockHttp::new());
        let (ctx, _sink) = make_streaming_ctx(http);
        ctx
    }

    #[tokio::test]
    async fn validation_rejects_short_query() {
        let ctx = validation_ctx();
        let tool = WebSearchTool::new(ctx);
        let err = tool
            .validate_input(&json!({ "query": "x" }), &fresh_ctx())
            .await
            .expect_err("too short");
        assert!(err.to_string().contains("at least 2 characters"));
    }

    #[tokio::test]
    async fn validation_rejects_both_domain_filters() {
        let ctx = validation_ctx();
        let tool = WebSearchTool::new(ctx);
        let err = tool
            .validate_input(
                &json!({
                    "query": "rust async",
                    "allowed_domains": ["docs.rs"],
                    "blocked_domains": ["spam.example"]
                }),
                &fresh_ctx(),
            )
            .await
            .expect_err("both domain filters must be rejected");
        // `ValidationError`'s Display prepends `invalid tool input: `; the
        // message bytes must match the TS string exactly.
        assert!(
            err.to_string().contains(
                "Error: Cannot specify both allowed_domains and blocked_domains in the same request"
            ),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn validation_allows_single_domain_filter() {
        let ctx = validation_ctx();
        let tool = WebSearchTool::new(ctx);
        // Only allowed_domains set — must pass.
        tool.validate_input(
            &json!({ "query": "rust async", "allowed_domains": ["docs.rs"] }),
            &fresh_ctx(),
        )
        .await
        .expect("single allowed_domains is valid");
        // Empty arrays on both sides are not "specifying both".
        tool.validate_input(
            &json!({ "query": "rust async", "allowed_domains": [], "blocked_domains": [] }),
            &fresh_ctx(),
        )
        .await
        .expect("empty domain arrays are valid");
    }

    #[tokio::test]
    async fn call_exposes_model_content_with_header_and_footer() {
        // Streaming path: a single leading-text block.
        let http = Arc::new(StreamingMockHttp::new());
        http.set_stream(vec![
            sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Here are results:" } })),
            sse(json!({ "type": "content_block_stop", "index": 0 })),
            sse(json!({ "type": "message_stop" })),
        ]);
        let (ctx, _sink) = make_streaming_ctx(http);
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("ok");
        // Model-facing text lives on `ToolCallResult.model_content` (the dispatch
        // uses it verbatim), NOT in `data`.
        let mc = res.model_content.as_deref().expect("model_content str");
        assert!(mc.starts_with("Web search results for query: \"rust async\"\n\n"));
        assert!(mc.contains("Here are results:"));
        assert!(mc.ends_with(
            "REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks."
        ));
        // Structured results array is still present for the TUI, and `data` is
        // pure metadata (no `model_content` key).
        assert!(res.data["results"].as_array().is_some());
        assert!(res.data.get("model_content").is_none());
    }

    #[tokio::test]
    async fn streaming_path_does_not_retry_internally() {
        // A successful streaming run must issue exactly ONE request (no internal
        // retry / no spurious fallback when the stream connects).
        let http = Arc::new(StreamingMockHttp::new());
        http.set_stream(vec![
            sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "ok" } })),
            sse(json!({ "type": "content_block_stop", "index": 0 })),
            sse(json!({ "type": "message_stop" })),
        ]);
        let (ctx, _sink) = make_streaming_ctx(http.clone());
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let _ = tool
            .call(json!({ "query": "rust" }), fresh_ctx(), tx)
            .await
            .expect("ok");
        assert_eq!(
            http.received_requests().len(),
            1,
            "successful stream must issue exactly one request (no self-retry)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn streaming_retries_hosted_search_on_529_overloaded() {
        // Parity 2.1.212: a connect-phase 529/overloaded on the hosted streaming
        // path is retried in place (CC's `queryModelWithStreaming` 529 loop),
        // NOT dropped to the non-retrying blocking fallback. First `stream_sse`
        // returns Err(529); second returns a success stream. `start_paused`
        // fast-forwards the `sle` backoff so the test runs instantly.
        struct Retry529ThenOk {
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait]
        impl HttpTransport for Retry529ThenOk {
            async fn request(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, HttpError> {
                // The blocking fallback must NOT be reached — the retry succeeds.
                Err(HttpError::InvalidRequest(
                    "blocking fallback must not run".into(),
                ))
            }
            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, HttpError> {
                let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // First connect: transient capacity 529 (== overloaded).
                    return Err(HttpError::Status {
                        status: 529,
                        body: "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}"
                            .into(),
                    });
                }
                // Second connect: a minimal success stream.
                struct S(u8);
                impl futures_util::Stream for S {
                    type Item = Result<protocol::SseEvent, HttpError>;
                    fn poll_next(
                        mut self: std::pin::Pin<&mut Self>,
                        _cx: &mut std::task::Context<'_>,
                    ) -> std::task::Poll<Option<Self::Item>> {
                        self.0 += 1;
                        let ev = match self.0 {
                            1 => {
                                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })
                            }
                            2 => {
                                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "recovered" } })
                            }
                            3 => json!({ "type": "content_block_stop", "index": 0 }),
                            4 => json!({ "type": "message_stop" }),
                            _ => return std::task::Poll::Ready(None),
                        };
                        std::task::Poll::Ready(Some(Ok(protocol::SseEvent {
                            event_type: ev.get("type").and_then(Value::as_str).map(str::to_string),
                            data: ev.to_string(),
                            id: None,
                        })))
                    }
                }
                Ok(Box::pin(S(0)))
            }
        }
        let http = Arc::new(Retry529ThenOk {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::default());
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            bus,
            vec![std::path::PathBuf::from("/tmp")],
        );
        ctx.http = http.clone() as Arc<dyn HttpTransport>;
        ctx.provider = Arc::new(tool_api::AnthropicRequestBuilder::new("test-key", None));
        ctx.default_model = "claude-sonnet-4-20250514".into();
        ctx.bus.attach_sink(sink.clone()).await;
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();
        let res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("529 must be retried, then succeed");
        assert!(!res.is_error, "retried run must succeed, not error");
        // The recovered stream's text survived into the results.
        let mc = res.model_content.as_deref().expect("model_content");
        assert!(mc.contains("recovered"), "recovered text missing: {mc}");
        // Exactly two stream_sse connects: the 529 + the successful retry (the
        // blocking fallback was never reached).
        assert_eq!(
            http.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must retry the stream connect exactly once after 529"
        );
        // COMPLETED (not FAILED) telemetry fired.
        let events = sink.events().await;
        let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tengu_tool_web_search_completed"));
        assert!(!names.contains(&"tengu_tool_web_search_failed"));
    }

    #[tokio::test]
    async fn copilot_session_uses_client_side_search_even_when_tool_provider_is_anthropic() {
        let http = Arc::new(StreamingMockHttp::new());
        http.enqueue_blocking(ok_blocking(
            200,
            r#"<a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fweather.example%2Fnow" class="result-link">Weather Now</a><td class="result-snippet">Sunny</td>"#,
        ));
        let (ctx, _sink) = make_streaming_ctx(http.clone());
        let tool = WebSearchTool::new(ctx);
        let mut use_ctx = fresh_ctx();
        use_ctx.options.model_profile = Some("github-copilot".to_string());
        let (tx, _rx) = progress_channel();

        let result = tool
            .call(json!({ "query": "weather now" }), use_ctx, tx)
            .await
            .expect("client-side search succeeds");

        let requests = http.received_requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .url
                .starts_with("https://lite.duckduckgo.com/lite/"),
            "Copilot sessions must use client-side search, got {}",
            requests[0].url
        );
        assert!(
            result.model_content.unwrap().contains("Weather Now"),
            "client-side results must be returned to the model"
        );
    }

    // ---- session-wide WebSearch budget (parity 2.1.212) --------------------

    // `AnalyticsValue` is already in scope via `use super::*` (the module's
    // top-level `telemetry::sink` import); the metadata asserts below use it.
    use platform_api::task_registry::{
        TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
        TaskRegistryHandle, TaskUpdatePatch,
    };

    /// Minimal `TaskRegistryHandle` exposing ONLY the session WebSearch counter;
    /// the 7 CRUD methods are unused error/empty stubs. `at(n)` presets the count;
    /// `increments()` reports how many times the gate bumped it.
    struct BudgetRegistry {
        count: std::sync::atomic::AtomicU32,
        increments: std::sync::atomic::AtomicU32,
    }
    impl BudgetRegistry {
        fn at(count: u32) -> Arc<Self> {
            Arc::new(Self {
                count: std::sync::atomic::AtomicU32::new(count),
                increments: std::sync::atomic::AtomicU32::new(0),
            })
        }
        fn increments(&self) -> u32 {
            self.increments.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl TaskRegistryHandle for BudgetRegistry {
        async fn create(&self, _: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn get(&self, _: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            Ok(None)
        }
        async fn list(&self, _: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError> {
            Ok(vec![])
        }
        async fn update(
            &self,
            _: &str,
            _: TaskUpdatePatch,
        ) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn set_status(&self, _: &str, _: &str) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn kill(&self, _: &str) -> Result<TaskRecord, TaskRegistryError> {
            Err(TaskRegistryError::Internal("unused".into()))
        }
        async fn output(
            &self,
            _: &str,
            _: Option<u64>,
        ) -> Result<TaskOutputChunk, TaskRegistryError> {
            Ok(TaskOutputChunk::default())
        }
        fn web_search_calls(&self) -> u32 {
            self.count.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn increment_web_search_calls(&self) {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.increments
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn parse_max_web_searches_mirrors_pe_int_min1_digits_only() {
        // Absent ⇒ default 200 (`ktu()`'s `?? 200`).
        assert_eq!(parse_max_web_searches(None), 200);
        // Plain digit strings.
        assert_eq!(parse_max_web_searches(Some("5")), 5);
        assert_eq!(parse_max_web_searches(Some("200")), 200);
        // Trimmed + optional leading '+' (both accepted by the digitsOnly regex).
        assert_eq!(parse_max_web_searches(Some("  10  ")), 10);
        assert_eq!(parse_max_web_searches(Some("+7")), 7);
        // `< 1` (min:1) ⇒ default.
        assert_eq!(parse_max_web_searches(Some("0")), 200);
        assert_eq!(parse_max_web_searches(Some("-4")), 200);
        // Non-integer / junk ⇒ default.
        assert_eq!(parse_max_web_searches(Some("abc")), 200);
        assert_eq!(parse_max_web_searches(Some("3.5")), 200);
        assert_eq!(parse_max_web_searches(Some("200abc")), 200);
        assert_eq!(parse_max_web_searches(Some("")), 200);
        // Over-`u32` (but within `u64`) clamps to `u32::MAX` (effectively
        // unlimited) — CC's `parseInt` yields a huge finite number, never caps.
        assert_eq!(parse_max_web_searches(Some("5000000000")), u32::MAX);
        // Over-`u64`: a 25-digit "unlimited" value overflows `u64` but still
        // matches CC's digitsOnly regex ⇒ must saturate to `u32::MAX`, NOT
        // regress to the 200 default.
        assert_eq!(
            parse_max_web_searches(Some("1000000000000000000000000")),
            u32::MAX
        );
        assert_eq!(
            parse_max_web_searches(Some("  +1000000000000000000000000  ")),
            u32::MAX
        );
        // A huge but *negative* / non-digit literal still ⇒ default.
        assert_eq!(
            parse_max_web_searches(Some("-1000000000000000000000000")),
            200
        );
    }

    #[test]
    fn budget_notice_is_byte_exact() {
        assert_eq!(
            web_search_budget_notice(200, 200),
            "Web search was not performed: this session has used its web search budget (200 of 200 WebSearch calls). Continue with the information already gathered instead of issuing more searches. If more searches are genuinely needed, ask the user to raise CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION."
        );
    }

    #[tokio::test]
    async fn web_search_over_budget_returns_notice_without_searching() {
        // Registry already AT the default budget (200) ⇒ 200 >= 200 ⇒ capped.
        let http = Arc::new(StreamingMockHttp::new());
        let (mut ctx, sink) = make_streaming_ctx(http.clone());
        ctx.bus.attach_sink(sink.clone()).await;
        let registry = BudgetRegistry::at(200);
        ctx.task_registry = Some(registry.clone() as Arc<dyn TaskRegistryHandle>);
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();

        let res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("cap path returns Ok");

        // No HTTP issued (neither stream nor blocking) — the search never runs.
        assert!(
            http.received_requests().is_empty(),
            "capped search must not touch the network"
        );
        // The counter is NOT bumped past the cap.
        assert_eq!(registry.increments(), 0);
        // `data` shape: results=[notice], durationSeconds:0, searchCount:0.
        assert_eq!(res.data["searchCount"], 0);
        assert_eq!(res.data["durationSeconds"], 0);
        let arr = res.data["results"].as_array().expect("results array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0], web_search_budget_notice(200, 200));
        assert!(!res.is_error);
        // Model text wraps the notice in the standard header + footer.
        let mc = res.model_content.as_deref().expect("model_content");
        assert!(mc.contains("Web search results for query: \"rust async\""));
        assert!(
            mc.contains("this session has used its web search budget (200 of 200 WebSearch calls)")
        );

        // `tengu_feature_bad` fired once with the session-cap metadata.
        let events = sink.events().await;
        let cap = events
            .iter()
            .find(|e| e.name == "tengu_feature_bad")
            .expect("tengu_feature_bad emitted");
        assert!(matches!(
            cap.metadata.get("feature_name"),
            Some(AnalyticsValue::String(s)) if s == "tool_web_search"
        ));
        assert!(matches!(
            cap.metadata.get("error_code"),
            Some(AnalyticsValue::String(s)) if s == "web_search_session_cap"
        ));
        assert!(matches!(
            cap.metadata.get("max_web_searches_per_session"),
            Some(AnalyticsValue::Int(200))
        ));
        // The normal per-query lifecycle telemetry is NOT emitted on the cap path.
        assert!(!events
            .iter()
            .any(|e| e.name == "tengu_tool_web_search_started"));
    }

    #[tokio::test]
    async fn web_search_under_budget_increments_and_searches() {
        // Registry BELOW the budget ⇒ the gate increments once and runs the search.
        let http = Arc::new(StreamingMockHttp::new());
        http.set_stream(vec![
            sse(json!({ "type": "message_start", "message": { "usage": { "input_tokens": 1, "output_tokens": 0 } } })),
            sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "server_tool_use", "id": "stu_1", "name": "web_search", "input": {} } })),
            sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"query\":\"q\"}" } })),
            sse(json!({ "type": "content_block_stop", "index": 0 })),
            sse(json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "web_search_tool_result", "tool_use_id": "stu_1", "content": [] } })),
            sse(json!({ "type": "content_block_stop", "index": 1 })),
            sse(json!({ "type": "message_stop" })),
        ]);
        let (mut ctx, _sink) = make_streaming_ctx(http.clone());
        let registry = BudgetRegistry::at(0);
        ctx.task_registry = Some(registry.clone() as Arc<dyn TaskRegistryHandle>);
        let tool = WebSearchTool::new(ctx);
        let (tx, _rx) = progress_channel();

        let _res = tool
            .call(json!({ "query": "rust async" }), anthropic_ctx(), tx)
            .await
            .expect("under-budget search runs");

        assert_eq!(registry.increments(), 1, "gate must increment exactly once");
        assert!(
            !http.received_requests().is_empty(),
            "under-budget search must hit the network"
        );
    }
}
