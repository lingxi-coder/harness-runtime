//! `WebSearchTool` — routes the agent's query through Anthropic's Messages
//! API with the `web-search-2025-03-05` anthropic-beta header and a
//! `web_search_20250305` tool block. Spec §7 web wire identifiers.
//!
//! Wire-locked constants (asserted byte-for-byte by `parity_web_tools.json`):
//! - `WEB_SEARCH_TOOL_BLOCK_TYPE = "web_search_20250305"` (tool block `type`)
//! - `WEB_SEARCH_TOOL_BLOCK_NAME = "web_search"` (tool block `name`)
//! - `WEB_SEARCH_MAX_USES = 8` (upstream `WebSearchTool.ts:80`)
//! - `WEB_SEARCH_DEFAULT_MAX_TOKENS = 4096`
//! Provider beta headers are supplied by the hosted-search session service.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;
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

pub use tool_api::hosted_search::SearchResultEntry;

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

fn map_web_search_result_text(data: &Value) -> Option<String> {
    let query = data.get("query")?.as_str()?;
    let entries = match data.get("results") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(entries)) => entries.as_slice(),
        _ => return None,
    };
    let results = entries
        .iter()
        .filter_map(|entry| match entry {
            Value::Null => None,
            Value::String(text) => Some(SearchResultEntry::Text(text.clone())),
            _ => Some(SearchResultEntry::Hit(entry.clone())),
        })
        .collect::<Vec<_>>();
    Some(build_model_content(query, &results))
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
    ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
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
                Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
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
                Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
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

/// Claude Code 2.1.287 WebSearch output union: commentary text or a search
/// result with a tool-use id and title/URL hits.
static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type":"object",
        "required":["query","results","durationSeconds"],
        "properties":{
            "query":{"type":"string"},
            "results":{"type":"array","items":{"oneOf":[
                {"type":"string"},
                {"type":"object","required":["tool_use_id","content"],"properties":{
                    "tool_use_id":{"type":"string"},
                    "content":{"type":"array","items":{"type":"object","required":["title","url"],"properties":{
                        "title":{"type":"string"},"url":{"type":"string"}
                    }}}
                }}
            ]}},
            "durationSeconds":{"type":"number"},
            "searchCount":{"type":"number"}
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
    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }
    fn map_result_text(&self, result: &Value) -> Option<String> {
        map_web_search_result_text(result)
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
        if !self.ctx.hosted_search.as_ref().is_some_and(|client| {
            client.supports(
                opts.model.as_deref().unwrap_or(&self.ctx.default_model),
                opts.model_profile.as_deref(),
            )
        }) {
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

        let request = tool_api::HostedSearchRequest {
            model: if ctx.options.main_loop_model.is_empty() {
                self.ctx.default_model.clone()
            } else {
                ctx.options.main_loop_model.clone()
            },
            profile: ctx.options.model_profile.clone(),
            query: parsed_input.query.clone(),
            allowed_domains: parsed_input.allowed_domains.clone().unwrap_or_default(),
            blocked_domains: parsed_input.blocked_domains.clone().unwrap_or_default(),
        };
        if !self
            .ctx
            .hosted_search
            .as_ref()
            .is_some_and(|client| client.supports_request(&request))
        {
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

        let client = self
            .ctx
            .hosted_search
            .as_ref()
            .expect("hosted support checked");

        let (progress, mut updates) = tokio::sync::mpsc::unbounded_channel();
        let search = client.search(request, progress);
        tokio::pin!(search);
        let mut count = 0;
        let result = loop {
            tokio::select! {
                result = &mut search => break result,
                Some(()) = updates.recv() => { count += 1; Self::emit_progress(&ctx, &tx, &parsed_input.query, count); }
            }
        };
        while updates.try_recv().is_ok() {
            count += 1;
            Self::emit_progress(&ctx, &tx, &parsed_input.query, count);
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(out) => Ok(self
                .build_success_result(
                    &invocation_id,
                    &parsed_input.query,
                    out.results,
                    out.searches,
                    out.input_tokens,
                    out.output_tokens,
                    elapsed_ms,
                )
                .await),
            Err(error) => Err(self
                .map_hosted_error(&invocation_id, error, elapsed_ms)
                .await),
        }
    }
}

impl WebSearchTool {
    /// Emit one incremental "searching" progress event, mirroring claude-code's
    /// per-search `onProgress({ type: 'query_update', query })`
    /// (`WebSearchTool.ts:344-354`). The payload carries the synthetic
    /// `search-progress-N` id and the query in `data` (TS keeps it on the event's
    /// `toolUseID`/`data`); here the channel key is the model's tool-use id when
    /// present, falling back to a fresh id (the channel key must be a real
    /// [`lingxi_core::types::ToolUseId`], unlike TS's free-form string). Best-effort
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
        ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
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

    async fn map_hosted_error(
        &self,
        invocation_id: &str,
        error: tool_api::HostedSearchError,
        elapsed_ms: u64,
    ) -> ToolError {
        let kind = if error.timeout {
            "timeout"
        } else if error.http_status.is_some() {
            "http_status"
        } else {
            "transport"
        };
        self.emit_failed(invocation_id, kind, error.http_status, elapsed_ms)
            .await;
        ToolError::Transport(format!("WebSearch: {}", error.message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::task_registry::{
        TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
        TaskRegistryHandle, TaskUpdatePatch,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tool_api::hosted_search::parse_response_content;
    use tool_api::progress::progress_channel;
    use tool_api::test_support::fresh_ctx;

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
    #[test]
    fn tool_max_result_size_matches_claude_limit() {
        let tool = WebSearchTool::new(hosted_context(Arc::new(Hosted::default())));
        assert_eq!(tool.max_result_size_chars(), 100_000);
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
    fn mod_replacement_uses_web_search_model_text_mapper() {
        let data = json!({
            "query":"rust async",
            "results":[null, "First summary.", {"content":[{"title":"Docs.rs","url":"https://docs.rs"}]}, 42]
        });
        let expected = build_model_content(
            "rust async",
            &[
                SearchResultEntry::Text("First summary.".into()),
                SearchResultEntry::Hit(
                    json!({"content":[{"title":"Docs.rs","url":"https://docs.rs"}]}),
                ),
                SearchResultEntry::Hit(json!(42)),
            ],
        );
        assert_eq!(map_web_search_result_text(&data), Some(expected));
        assert_eq!(
            map_web_search_result_text(&json!({"query":"q"})),
            Some(build_model_content("q", &[]))
        );
    }

    #[test]
    fn mod_replacement_validates_web_search_output_shape() {
        let valid = json!({
            "query":"rust async", "durationSeconds":0.5, "searchCount":1,
            "results":["Summary",{"tool_use_id":"stu_1","content":[
                {"title":"Docs.rs","url":"https://docs.rs"}
            ]}]
        });
        assert!(tool_api::output_schema::validate(&OUTPUT_SCHEMA, &valid).is_ok());
        let mut invalid = valid;
        invalid["results"][1]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("url");
        assert!(tool_api::output_schema::validate(&OUTPUT_SCHEMA, &invalid).is_err());
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
    fn ignores_thinking_blocks() {
        let blocks = vec![json!({ "type": "thinking", "thinking": "let me think" })];
        let parsed = parse_response_content(&blocks);
        assert!(parsed.is_empty());
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
    #[derive(Default)]
    struct Hosted {
        calls: AtomicUsize,
        seen: Mutex<Vec<tool_api::HostedSearchRequest>>,
        error: Option<tool_api::HostedSearchError>,
    }
    #[async_trait]
    impl tool_api::HostedWebSearchClient for Hosted {
        fn supports(&self, _: &str, profile: Option<&str>) -> bool {
            profile != Some("client-only")
        }
        async fn search(
            &self,
            request: tool_api::HostedSearchRequest,
            progress: tokio::sync::mpsc::UnboundedSender<()>,
        ) -> Result<tool_api::HostedSearchOutput, tool_api::HostedSearchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(request);
            progress.send(()).unwrap();
            tokio::task::yield_now().await;
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok(tool_api::HostedSearchOutput {
                results: vec![SearchResultEntry::Hit(
                    json!({"tool_use_id":"search","content":[{"title":"Rust", "url":"https://rust-lang.org"}]}),
                )],
                searches: 1,
                input_tokens: 7,
                output_tokens: 9,
            })
        }
    }
    fn hosted_context(hosted: Arc<Hosted>) -> BuiltinToolContext {
        let mut ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec!["/tmp".into()],
        );
        ctx.hosted_search = Some(hosted);
        ctx
    }
    #[tokio::test]
    async fn hosted_search_uses_live_profile_domains_and_reports_progress() {
        let hosted = Arc::new(Hosted::default());
        let tool = WebSearchTool::new(hosted_context(hosted.clone()));
        let mut ctx = fresh_ctx();
        ctx.options.main_loop_model = "live-model".into();
        ctx.options.model_profile = Some("live-profile".into());
        let (tx, mut rx) = progress_channel();
        let result = tool
            .call(
                json!({"query":"rust language","allowed_domains":["rust-lang.org"]}),
                ctx,
                tx,
            )
            .await
            .unwrap();
        assert_eq!(hosted.calls.load(Ordering::SeqCst), 1);
        let seen = hosted.seen.lock().unwrap();
        assert_eq!(seen[0].model, "live-model");
        assert_eq!(seen[0].profile.as_deref(), Some("live-profile"));
        assert_eq!(seen[0].allowed_domains, vec!["rust-lang.org"]);
        assert!(rx.try_recv().is_ok());
        assert!(result
            .model_content
            .unwrap()
            .contains("https://rust-lang.org"));
        assert_eq!(result.data["searchCount"], 1);
    }
    #[tokio::test]
    async fn hosted_failure_does_not_trigger_a_second_model_request() {
        let hosted = Arc::new(Hosted {
            error: Some(tool_api::HostedSearchError {
                message: "unknown generation outcome".into(),
                http_status: None,
                timeout: false,
            }),
            ..Default::default()
        });
        let tool = WebSearchTool::new(hosted_context(hosted.clone()));
        let (tx, _) = progress_channel();
        assert!(tool
            .call(json!({"query":"rust language"}), fresh_ctx(), tx)
            .await
            .is_err());
        assert_eq!(hosted.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn budget_prevents_search_and_does_not_increment() {
        let hosted = Arc::new(Hosted::default());
        let mut ctx = hosted_context(hosted.clone());
        let registry = BudgetRegistry::at(u32::MAX);
        ctx.task_registry = Some(registry.clone());
        let (tx, _) = progress_channel();
        let result = WebSearchTool::new(ctx)
            .call(json!({"query":"rust language"}), fresh_ctx(), tx)
            .await
            .unwrap();
        assert_eq!(result.data["searchCount"], 0);
        assert_eq!(hosted.calls.load(Ordering::SeqCst), 0);
        assert_eq!(registry.increments(), 0);
    }
    #[tokio::test]
    async fn budget_counts_successful_admission_once() {
        let hosted = Arc::new(Hosted::default());
        let mut ctx = hosted_context(hosted.clone());
        let registry = BudgetRegistry::at(0);
        ctx.task_registry = Some(registry.clone());
        let (tx, _) = progress_channel();
        WebSearchTool::new(ctx)
            .call(json!({"query":"rust language"}), fresh_ctx(), tx)
            .await
            .unwrap();
        assert_eq!(registry.increments(), 1);
        assert_eq!(hosted.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn hosted_error_telemetry_preserves_status_and_timeout() {
        #[derive(Default)]
        struct Sink(Mutex<Vec<(String, LogEventMetadata)>>);
        #[async_trait]
        impl telemetry::AnalyticsSink for Sink {
            async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
                self.0.lock().unwrap().push((name.into(), metadata));
            }
            async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
                self.log_event(name, metadata).await;
            }
            fn name(&self) -> &str {
                "search-test"
            }
        }
        for (status, timeout, kind) in [
            (Some(429), false, "http_status"),
            (Some(401), false, "http_status"),
            (None, true, "timeout"),
            (None, false, "transport"),
        ] {
            let hosted = Arc::new(Hosted {
                error: Some(tool_api::HostedSearchError {
                    message: "failed".into(),
                    http_status: status,
                    timeout,
                }),
                ..Default::default()
            });
            let ctx = hosted_context(hosted.clone());
            let sink = Arc::new(Sink::default());
            ctx.bus.attach_sink(sink.clone()).await;
            let (tx, _rx) = progress_channel();
            assert!(WebSearchTool::new(ctx)
                .call(json!({"query":"rust language"}), fresh_ctx(), tx)
                .await
                .is_err());
            assert_eq!(hosted.calls.load(Ordering::SeqCst), 1);
            let events = sink.0.lock().unwrap();
            let failed = events
                .iter()
                .find(|(name, _)| name == WEB_SEARCH_FAILED)
                .expect("failure telemetry");
            let metadata = serde_json::to_value(&failed.1).unwrap();
            assert_eq!(metadata["error_kind"], kind);
            assert_eq!(metadata["status"], serde_json::to_value(status).unwrap());
        }
    }
}
