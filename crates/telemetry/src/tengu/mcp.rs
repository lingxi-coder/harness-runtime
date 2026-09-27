//! `tengu_mcp_*` **analytics-event** schemas (2.1.252 registry/catalog/connect
//! slice, plus 2.1.251 §20a/§20b and §11 discovery-cache).
//!
//! ## Scope: 44 of the 55 provider-neutral oracle MCP analytics events
//!
//! An earlier revision of this doc claimed the two originally-ported names
//! were "the ONLY two confirmed real `tengu_mcp_*` ANALYTICS events at the
//! oracle; everything else with that prefix is a Statsig feature-flag name".
//! **That claim was false and has been retracted.** Scanning the 2.1.251
//! image for the analytics-bus call shape
//! `(?<![A-Za-z0-9_$])s\("tengu_mcp_[a-z0-9_]+"` returns **63 distinct event
//! names** in the local 2.1.252 oracle image. Of those, 10 are explicitly
//! excluded from this provider-neutral registry slice:
//!
//! - 6 first-party / IDE names (`tengu_mcp_ide_server_connection_*` and the
//!   other Anthropic- or IDE-specific families)
//! - 4 `tengu_mcp_channel_*` names
//!
//! That leaves **55 provider-neutral events**, among them
//! `tengu_mcp_server_connection_succeeded` / `_failed`,
//! `tengu_mcp_list_changed`, `tengu_mcp_listen_reopen`,
//! `tengu_mcp_sdk_generation`, `tengu_mcp_oauth_flow_start` / `_success` /
//! `_failure` / `_error`, `tengu_mcp_registry_fetch`,
//! `tengu_mcp_elicitation_shown` / `_response`,
//! and `tengu_mcp_first_party_auto_auth`.
//!
//! It IS separately true that a full-binary grep for the bare prefix
//! `tengu_mcp_` turns up 100+ hits of which many are **Statsig
//! feature-flag** names read via `I(name, default)` / `Ot(name, e)` (e.g.
//! `tengu_mcp_protocol_negotiation_http`,
//! `tengu_mcp_normalize_root_combinators`,
//! `tengu_mcp_connect_timeout_retry`). Those are ported as plain `&str`
//! constants passed to [`telemetry::flag_bool`] at their call sites and do
//! NOT belong in this count-locked event registry. But "not every
//! `tengu_mcp_*` string is an event" does not license "only two strings are
//! events", and conflating the two produced both the retracted claim below
//! and an under-modelled [`DegradedReason`].
//!
//! **This module deliberately ports 44 of the 55 provider-neutral names.** The
//! remaining 11 are an OPEN parity gap, not a closed one. Do not read
//! `NAMES.len()`, [`crate::tengu::ALL_EVENT_NAMES`], or the
//! `tengu_events.json` parity fixture as evidence that MCP analytics is
//! complete.
//!
//! The exact remaining 11 provider-neutral oracle names not registered here are:
//! `tengu_mcp_instructions_pool_change`,
//! `tengu_mcp_dropped_tools_pool_change`,
//! `tengu_mcp_skills_funnel`,
//! `tengu_mcp_arg_trailing_invoke_suffix`,
//! `tengu_mcp_description_contains_toolcall_xml`,
//! `tengu_mcp_dialog_choice`,
//! `tengu_mcp_multidialog_choice`,
//! `tengu_mcp_proxy_needs_approval_retry`,
//! `tengu_mcp_registry_fetch`,
//! `tengu_mcp_sdk_generation`,
//! and `tengu_mcp_tripwire`.
//!
//! ## Newly re-extracted 2.1.252 call sites in this parity pass
//!
//! The local 2.1.252 oracle dump was re-extracted before this edit. Exact
//! payload literals added in this pass include:
//!
//! - `tengu_mcp_add` — `s("tengu_mcp_add",{type:c(M),scope:c(A),source:w("command"),transport:c(M),transportExplicit:T,looksLikeUrl:H})`,
//!   plus the JSON import arm `s("tengu_mcp_add",{scope:c(m),source:w("json"),type:h})`
//!   and the Desktop import arm `s("tengu_mcp_add",{scope:c(g),platform:c(a),source:w("desktop")})`.
//! - `tengu_mcp_delete` / `_get` / `_list` / `_login` / `_logout` —
//!   `s("tengu_mcp_delete",{name:s,scope:c(C)})`, `s("tengu_mcp_get",{name:s})`,
//!   `s("tengu_mcp_list",{})`, `s("tengu_mcp_login",{})`,
//!   and `s("tengu_mcp_logout",{})`.
//! - `tengu_mcp_command_inline` —
//!   `s("tengu_mcp_command_inline",{action:c(S)})`.
//! - `tengu_mcp_elicitation_shown` / `_response` —
//!   `s("tengu_mcp_elicitation_shown",{mode:c(ae)})` and
//!   `s("tengu_mcp_elicitation_response",{mode:c(ae),action:c(...)})`.
//! - `tengu_mcp_input_missing_required` —
//!   `s("tengu_mcp_input_missing_required",{toolName:Un(e.name),isMcp:!0,toolUseID:ve(t),messageID:ve(_),toolInputSizeBytes:W,requiredCount:fn.requiredCount,missingCount:fn.missingCount,presentKeyCount:fn.presentKeyCount,maxStringValueLen:fn.maxStringValueLen,hasPseudoTagDebris:fn.hasPseudoTagDebris,...rpe(o.agentContext),queryChainId:ve(o.queryTracking?.chainId),queryDepth:o.queryTracking?.depth,...A&&{mcpServerType:c(A)},...x&&{mcpServerBaseUrl:gg(x)},...C&&{requestId:ve(C)},...k6(e.name,M),...e.mcpInfo?.pluginTelemetry})`.
//! - `tengu_mcp_large_result_handled` — truncation arms emit
//!   `{outcome,reason,sizeEstimateTokens}` and the persisted-file arm also emits
//!   `{persistedSizeChars,resultType,blockCount,persistedAs}`.
//! - `tengu_mcp_pending_call` —
//!   `s("tengu_mcp_pending_call",{requestedCount:o.length,connectedCount:M.length,cachedCount:F.length,failedCount:U.length,pendingCount:B.length,needsAuthCount:W.length,disabledCount:z.length,unconfiguredCount:pe.length,unknownCount:me.length,waitMs:A,matched:ge,matchType:w("wait"),success:ge})`.
//! - `tengu_mcp_servers` —
//!   `s("tengu_mcp_servers",{enterprise,global,project,user,plugin,agent,claudeai})`.
//! - `tengu_mcp_tool_result_ended_turn` —
//!   `s("tengu_mcp_tool_result_ended_turn",{queryChainId:B,queryDepth:W,source:c(e)})`.
//! - `tengu_mcp_tools_commands_loaded` —
//!   `s("tengu_mcp_tools_commands_loaded",{tools_count:_.length,commands_count:P.length,commands_metadata_length:U})`.
//! - `tengu_mcp_tools_refreshed_mid_turn` —
//!   `s("tengu_mcp_tools_refreshed_mid_turn",{oldMcpCount:fo,newMcpCount:di,recovered:fo===0&&di>0})`.
//! - `tengu_mcp_oauth_flow_failure` — XAA-only:
//!   `s("tengu_mcp_oauth_flow_failure",{authMethod:w("xaa"),xaaFailureStage:c(C),idTokenCacheHit:S})`.
//! - `tengu_mcp_session_expired` — at least
//!   `{errorCode?,transportType,...identitySpread,mcpServerName?,mcpToolName?}`.
//! - `tengu_mcp_list_paginated` —
//!   `s("tengu_mcp_list_paginated",{method:c(e),pageCount:t,itemCount:o,outcome:c(r),source:c(d)})`
//!   and the later helper
//!   `s("tengu_mcp_list_paginated",{method:c(e),pageCount:t,itemCount:r,outcome:c(o)})`.
//! - `tengu_mcp_reconcile` —
//!   `s("tengu_mcp_reconcile",{caller:_,desiredCount:j.size,currentCount:B.size,toRemoveCount:oe.length,toAddCount:G.length,toReplaceCount:De.length,retainedPluginCount:re.size})`.
//!
//! ## The previously ported events
//!
//! - `tengu_mcp_start` — emitted by the `mcp serve` CLI path once the local
//!   stdio server runtime is initialized and just before it enters the request
//!   loop. Oracle 2.1.252 call site:
//!   `s("tengu_mcp_start",{transport:c("stdio")})`.
//! - `tengu_mcp_server_config_invalid` — `mcp/src/connection.rs`'s
//!   `Transport::connect_time_url_error` / `config_error` paths already
//!   compute the oracle's `INVALID_CONFIG` classification. Oracle call
//!   site: `s("tengu_mcp_server_config_invalid",{transportType:c(t.type
//!   ??"stdio"),field:w("url"),source:w(t.configError?"loader":"connect")})`.
//! - `tengu_mcp_server_connection_succeeded` — emitted after a live connect
//!   completes and its catalog is loaded. Oracle 2.1.252 carries
//!   `connectionDurationMs`, `transportType`, `scope`, `isPlugin`, and, on the
//!   modern path, negotiation/probe fields.
//! - `tengu_mcp_server_connection_failed` — emitted on connect failure, from
//!   early classified failures and the timed live-connect path. Oracle 2.1.252
//!   carries `transportType`, `errorCode`, and on the timed path also
//!   `connectionDurationMs`, `errorClassName`, `errorMessageHash`, and the
//!   same connection-shape fields the success event carries.
//! - `tengu_mcp_tools_listed` — emitted once per successful `tools/list`.
//!   Oracle call site: `s("tengu_mcp_tools_listed",{transportType:c(e.config.type
//!   ??"stdio"),listDurationMs:Date.now()-o,toolCount:L.length,
//!   alwaysLoadCount:Q(L,(E)=>E.alwaysLoad===!0),discoverySource:c(r),..._,
//!   mcpServerName:EA(ln(e.name),HT(e.name,e.config))})`.
//! - `tengu_mcp_degraded` — see [`DegradedReason`].
//! - `tengu_mcp_discovery_source` — §11 discovery-cache observability. See
//!   [`DiscoverySourcePayload`] for the two oracle call sites (a fresh/stale
//!   HIT, and a MISS gated by `Ko`) and `mcp::discovery_cache`'s module doc
//!   for what this port actually wires (today: the MISS side only).
//! - `tengu_mcp_list_changed` — emitted after a successful
//!   `tools/prompts/resources` refresh triggered by an inbound
//!   `notifications/*/list_changed`. For `tools`, the oracle also carries
//!   `newCount` and, when the prior list is known, `previousCount`.
//! - `tengu_mcp_resource_templates_fetched` — emitted when
//!   `resources/templates/list` succeeds, carrying `template_count`.
//! - `tengu_mcp_listen_reopen` — emitted when the registry opens, reopens,
//!   gives up, exhausts its reopen budget, or parks a modern
//!   `subscriptions/listen` stream, carrying the privacy-safe server key hash
//!   plus outcome/attempt count/trigger.
//! - `tengu_mcp_reset_mcpjson_choices` — emitted at CLI command entry for
//!   `mcp reset-project-choices`; empty payload.
//! - `tengu_mcp_tool_auto_backgrounded` — emitted by the MCP tool dispatcher
//!   when a long-running call is moved into the background. Its historical
//!   constant remains in [`super::tool`], but the wire prefix makes this MCP
//!   analytics block its registry home.
//!
//! ## `listDurationMs` measures `tools/list` ALONE
//!
//! Oracle `yt` (@182326900): `let d=Date.now(), …, h=await …"tools/list"…,
//! _=yn(e,h,d,"live",r)`, and inside `yn`: `listDurationMs:Date.now()-o`
//! with `o` bound to that `d`. The timer opens immediately before the
//! `tools/list` round-trip and closes immediately after it — no
//! `resources/list` or `prompts/list` call sits inside the window.
//!
//! ## `mcpServerName` is GATED, and absent for ordinary servers
//!
//! The oracle does NOT attach the raw server name unconditionally. It
//! computes `P = EA(ln(e.name), HT(e.name, e.config))` where
//! `EA(n,e){return e?Vo(n):void 0}` (@153570482) — returning `undefined`,
//! which the object spread DROPS, unless the predicate holds. The predicate
//! `HT(e,t)` (@156027690) is:
//!
//! ```text
//! function HT(e,t){
//!   if(t===void 0){ if(Z3t.has(e))return!0; return $M(void 0,void 0) }
//!   if(dM(e,t))return!0;                                   // first-party stdio
//!   if("url"in t&&X7e(t.url)&&ln(e)===uy)return!0;         // first-party url
//!   return $M(t.type,vAe(t))
//! }
//! function dM(e,t){return C6(t)&&(OH(e)||F6(e))}           // stdio AND name in {Ed,e0}
//! function $M(e,t){
//!   if(process.env.CLAUDE_CODE_ENTRYPOINT==="local-agent")return!0;
//!   if(e==="claudeai-proxy")return!0;
//!   if(t&&tM(t))return!0;                                   // official registry url
//!   if(t&&rA(t))return!0;                                   // anthropic /v1/design/ url
//!   return!1
//! }
//! ```
//!
//! Every arm is a FIRST-PARTY test: a built-in Anthropic stdio server
//! (`OH`/`F6` compare `ln(name)` against two fixed constants), a
//! `claudeai-proxy` transport, an official-registry URL, or an
//! Anthropic-issued design URL. For an ordinary user-configured server —
//! `acme-internal-payroll` in someone's `.mcp.json` — `HT` is **false** and
//! the oracle emits `tengu_mcp_tools_listed` / `tengu_mcp_degraded` with no
//! `mcpServerName` key at all.
//!
//! Modelling the field as required-and-always-populated therefore did not
//! just diverge from the oracle, it turned a private server name into an
//! analytics dimension on every connect. The field is now
//! `Option<Verified>`; see [`server_name_gate`] for what this port can and
//! cannot evaluate.
//!
//! ## Privacy-safe `..._` identity spread
//!
//! Every per-server emission above also spreads `_ = Xe(e.config, e.name)`
//! (@182372738): `function Xe(e,t){let o=Og(e),r=t?{mcpServerKeyHash:tP(t)}:{};
//! if(o)return{mcpServerBaseUrl:o,...r};return r}` — i.e. an optional
//! `mcpServerBaseUrl` plus `mcpServerKeyHash`, the hashed identifier the
//! oracle carries in place of the raw name. `tP` is SHA-256 of the configured
//! server name truncated to 12 lowercase hex characters; the misleadingly
//! named base-URL dimension is likewise a 12-character SHA-256 prefix of the
//! credential/query/fragment-free normalized URL. Producers must never place
//! either raw value in these analytics dimensions.

use crate::pii::Verified;
use serde::{Deserialize, Serialize};

/// `tengu_mcp_server_config_invalid` — a server's config failed the
/// connect-time (or loader-time) URL/shape re-validation.
pub const START: &str = "tengu_mcp_start";
/// `tengu_mcp_server_config_invalid` — a server's config failed the
/// connect-time (or loader-time) URL/shape re-validation.
pub const SERVER_CONFIG_INVALID: &str = "tengu_mcp_server_config_invalid";
/// `tengu_mcp_server_connection_succeeded` — a live connect plus catalog load
/// completed.
pub const SERVER_CONNECTION_SUCCEEDED: &str = "tengu_mcp_server_connection_succeeded";
/// `tengu_mcp_server_connection_failed` — a connect attempt failed.
pub const SERVER_CONNECTION_FAILED: &str = "tengu_mcp_server_connection_failed";
/// `tengu_mcp_tools_listed` — a `tools/list` round-trip completed and the
/// tool set was bound.
pub const TOOLS_LISTED: &str = "tengu_mcp_tools_listed";
/// `tengu_mcp_degraded` — one of the §20a tool-schema classifications (or
/// the process-global validator-unavailable fallback) fired at least once.
pub const DEGRADED: &str = "tengu_mcp_degraded";
/// `tengu_mcp_discovery_source` — §11 discovery-cache observability. See
/// [`DiscoverySourcePayload`].
pub const DISCOVERY_SOURCE: &str = "tengu_mcp_discovery_source";
/// `tengu_mcp_list_changed` — a `list_changed` refresh completed.
pub const LIST_CHANGED: &str = "tengu_mcp_list_changed";
/// `tengu_mcp_resource_templates_fetched` — `resources/templates/list`
/// succeeded.
pub const RESOURCE_TEMPLATES_FETCHED: &str = "tengu_mcp_resource_templates_fetched";
/// `tengu_mcp_listen_reopen` — a modern `subscriptions/listen` stream opened,
/// reopened, gave up, exhausted budget, or parked.
pub const LISTEN_REOPEN: &str = "tengu_mcp_listen_reopen";
/// `tengu_mcp_reset_mcpjson_choices` — CLI `mcp reset-project-choices`
/// command-entry event, empty payload.
pub const RESET_MCPJSON_CHOICES: &str = "tengu_mcp_reset_mcpjson_choices";
/// `tengu_mcp_auth_config_authenticate` — a user-triggered authenticate action
/// started for an MCP server.
pub const AUTH_CONFIG_AUTHENTICATE: &str = "tengu_mcp_auth_config_authenticate";
/// `tengu_mcp_auth_config_clear` — a user-triggered credential-clear action
/// started for an MCP server.
pub const AUTH_CONFIG_CLEAR: &str = "tengu_mcp_auth_config_clear";
/// `tengu_mcp_oauth_browser_open` — the authorization URL was surfaced and a
/// browser open was attempted or skipped.
pub const OAUTH_BROWSER_OPEN: &str = "tengu_mcp_oauth_browser_open";
/// `tengu_mcp_oauth_flow_start` — standard MCP OAuth flow started.
pub const OAUTH_FLOW_START: &str = "tengu_mcp_oauth_flow_start";
/// `tengu_mcp_oauth_flow_success` — standard MCP OAuth flow completed.
pub const OAUTH_FLOW_SUCCESS: &str = "tengu_mcp_oauth_flow_success";
/// `tengu_mcp_oauth_flow_error` — standard MCP OAuth flow errored or was
/// canceled.
pub const OAUTH_FLOW_ERROR: &str = "tengu_mcp_oauth_flow_error";
/// `tengu_mcp_oauth_refresh_success` — a standard MCP OAuth refresh
/// succeeded.
pub const OAUTH_REFRESH_SUCCESS: &str = "tengu_mcp_oauth_refresh_success";
/// `tengu_mcp_oauth_refresh_failure` — a standard MCP OAuth refresh failed.
pub const OAUTH_REFRESH_FAILURE: &str = "tengu_mcp_oauth_refresh_failure";
/// `tengu_mcp_oauth_token_persist_failed` — token persistence failed.
pub const OAUTH_TOKEN_PERSIST_FAILED: &str = "tengu_mcp_oauth_token_persist_failed";
/// `tengu_mcp_oauth_issuer_echo_mismatch` — issuer echo validation observed a
/// mismatch.
pub const OAUTH_ISSUER_ECHO_MISMATCH: &str = "tengu_mcp_oauth_issuer_echo_mismatch";
/// `tengu_mcp_server_needs_auth` — a server connect attempt concluded that the
/// server needs authentication.
pub const SERVER_NEEDS_AUTH: &str = "tengu_mcp_server_needs_auth";
/// `tengu_mcp_tool_call_auth_error` — a tool call hit an auth failure after
/// retry handling.
pub const TOOL_CALL_AUTH_ERROR: &str = "tengu_mcp_tool_call_auth_error";
/// `tengu_mcp_add` — CLI `mcp add` command entry.
pub const ADD: &str = "tengu_mcp_add";
/// `tengu_mcp_delete` — CLI `mcp remove` command entry.
pub const DELETE: &str = "tengu_mcp_delete";
/// `tengu_mcp_get` — CLI `mcp get` command entry.
pub const GET: &str = "tengu_mcp_get";
/// `tengu_mcp_list` — CLI `mcp list` command entry.
pub const LIST: &str = "tengu_mcp_list";
/// `tengu_mcp_login` — CLI `mcp login` command entry.
pub const LOGIN: &str = "tengu_mcp_login";
/// `tengu_mcp_logout` — CLI `mcp logout` command entry.
pub const LOGOUT: &str = "tengu_mcp_logout";
/// `tengu_mcp_command_inline` — inline `/mcp` control handling.
pub const COMMAND_INLINE: &str = "tengu_mcp_command_inline";
/// `tengu_mcp_elicitation_shown` — an inbound elicitation was surfaced.
pub const ELICITATION_SHOWN: &str = "tengu_mcp_elicitation_shown";
/// `tengu_mcp_elicitation_response` — an inbound elicitation was answered.
pub const ELICITATION_RESPONSE: &str = "tengu_mcp_elicitation_response";
/// `tengu_mcp_input_missing_required` — MCP input omitted required keys.
pub const INPUT_MISSING_REQUIRED: &str = "tengu_mcp_input_missing_required";
/// `tengu_mcp_large_result_handled` — large-result truncation/persist handling.
pub const LARGE_RESULT_HANDLED: &str = "tengu_mcp_large_result_handled";
/// `tengu_mcp_pending_call` — pending MCP-server readiness telemetry.
pub const PENDING_CALL: &str = "tengu_mcp_pending_call";
/// `tengu_mcp_servers` — scope-bucket inventory telemetry.
pub const SERVERS: &str = "tengu_mcp_servers";
/// `tengu_mcp_tool_result_ended_turn` — an MCP tool result ended the turn.
pub const TOOL_RESULT_ENDED_TURN: &str = "tengu_mcp_tool_result_ended_turn";
/// `tengu_mcp_tools_commands_loaded` — prefetched MCP tool/command counts.
pub const TOOLS_COMMANDS_LOADED: &str = "tengu_mcp_tools_commands_loaded";
/// `tengu_mcp_tools_refreshed_mid_turn` — mid-turn MCP tool refresh delta.
pub const TOOLS_REFRESHED_MID_TURN: &str = "tengu_mcp_tools_refreshed_mid_turn";
/// `tengu_mcp_oauth_flow_failure` — XAA-only OAuth flow failure telemetry.
pub const OAUTH_FLOW_FAILURE: &str = "tengu_mcp_oauth_flow_failure";
/// `tengu_mcp_session_expired` — a tool call hit a stale session or dropped
/// response.
pub const SESSION_EXPIRED: &str = "tengu_mcp_session_expired";
/// `tengu_mcp_list_paginated` — paginated MCP list traversal telemetry.
pub const LIST_PAGINATED: &str = "tengu_mcp_list_paginated";
/// `tengu_mcp_reconcile` — live registry diff telemetry.
pub const RECONCILE: &str = "tengu_mcp_reconcile";

/// Registry block — order is locked (append-only). Consumed by
/// [`crate::tengu::ALL_EVENT_NAMES`].
pub const NAMES: &[&str] = &[
    SERVER_CONFIG_INVALID,
    SERVER_CONNECTION_SUCCEEDED,
    SERVER_CONNECTION_FAILED,
    TOOLS_LISTED,
    DEGRADED,
    DISCOVERY_SOURCE,
    LIST_CHANGED,
    RESOURCE_TEMPLATES_FETCHED,
    LISTEN_REOPEN,
    RESET_MCPJSON_CHOICES,
    super::tool::MCP_TOOL_AUTO_BACKGROUNDED,
    START,
    AUTH_CONFIG_AUTHENTICATE,
    AUTH_CONFIG_CLEAR,
    OAUTH_BROWSER_OPEN,
    OAUTH_FLOW_START,
    OAUTH_FLOW_SUCCESS,
    OAUTH_FLOW_ERROR,
    OAUTH_REFRESH_SUCCESS,
    OAUTH_REFRESH_FAILURE,
    OAUTH_TOKEN_PERSIST_FAILED,
    OAUTH_ISSUER_ECHO_MISMATCH,
    SERVER_NEEDS_AUTH,
    TOOL_CALL_AUTH_ERROR,
    ADD,
    DELETE,
    GET,
    LIST,
    LOGIN,
    LOGOUT,
    COMMAND_INLINE,
    ELICITATION_SHOWN,
    ELICITATION_RESPONSE,
    INPUT_MISSING_REQUIRED,
    LARGE_RESULT_HANDLED,
    PENDING_CALL,
    SERVERS,
    TOOL_RESULT_ENDED_TURN,
    TOOLS_COMMANDS_LOADED,
    TOOLS_REFRESHED_MID_TURN,
    OAUTH_FLOW_FAILURE,
    SESSION_EXPIRED,
    LIST_PAGINATED,
    RECONCILE,
];

/// Payload for [`START`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartPayload {
    /// Current local `mcp serve` transport. The oracle emits the bare
    /// low-cardinality transport string under the key `transport`.
    pub transport: Verified,
}

/// Payload for [`AUTH_CONFIG_AUTHENTICATE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfigAuthenticatePayload {
    pub was_authenticated: bool,
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`AUTH_CONFIG_CLEAR`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfigClearPayload {
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`OAUTH_BROWSER_OPEN`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthBrowserOpenPayload {
    pub success: bool,
    pub headless: bool,
    pub platform: Verified,
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`OAUTH_FLOW_START`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthFlowStartPayload {
    pub flow_attempt_id: Verified,
    pub is_oauth_flow: bool,
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`OAUTH_FLOW_SUCCESS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthFlowSuccessPayload {
    pub flow_attempt_id: Verified,
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`OAUTH_FLOW_ERROR`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthFlowErrorPayload {
    pub flow_attempt_id: Verified,
    pub reason: Verified,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<Verified>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`OAUTH_REFRESH_SUCCESS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthRefreshSuccessPayload {
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`OAUTH_REFRESH_FAILURE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthRefreshFailurePayload {
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
    pub reason: Verified,
}

/// Payload for [`OAUTH_TOKEN_PERSIST_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthTokenPersistFailedPayload {
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
    pub reason: Verified,
}

/// `tengu_mcp_oauth_issuer_echo_mismatch`'s `site`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthIssuerEchoSite {
    Rfc9728Chain,
    RefreshRediscovery,
}

impl OAuthIssuerEchoSite {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Rfc9728Chain => "rfc9728_chain",
            Self::RefreshRediscovery => "refresh_rediscovery",
        }
    }
}

/// `tengu_mcp_oauth_issuer_echo_mismatch`'s `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthIssuerEchoMode {
    Observe,
    Enforce,
}

impl OAuthIssuerEchoMode {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

/// `tengu_mcp_oauth_issuer_echo_mismatch`'s `originRelation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthIssuerOriginRelation {
    CrossOrigin,
    SameOrigin,
    Unparseable,
}

impl OAuthIssuerOriginRelation {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::CrossOrigin => "cross_origin",
            Self::SameOrigin => "same_origin",
            Self::Unparseable => "unparseable",
        }
    }
}

/// `tengu_mcp_oauth_issuer_echo_mismatch`'s `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthIssuerEchoOutcome {
    Denied,
    Proceeded,
}

impl OAuthIssuerEchoOutcome {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Proceeded => "proceeded",
        }
    }
}

/// Payload for [`OAUTH_ISSUER_ECHO_MISMATCH`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthIssuerEchoMismatchPayload {
    pub site: OAuthIssuerEchoSite,
    pub mode: OAuthIssuerEchoMode,
    pub origin_relation: OAuthIssuerOriginRelation,
    pub outcome: OAuthIssuerEchoOutcome,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub mismatch_facets: Vec<Verified>,
    pub expected_issuer_hash: Verified,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received_issuer_hash: Option<Verified>,
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`SERVER_NEEDS_AUTH`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerNeedsAuthPayload {
    pub transport_type: Verified,
    pub mcp_server_key_hash: Verified,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Verified>,
}

/// `tengu_mcp_tool_call_auth_error`'s `authErrorKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallAuthErrorKind {
    NotConnected,
    TokenExpired,
}

impl ToolCallAuthErrorKind {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::NotConnected => "not_connected",
            Self::TokenExpired => "token_expired",
        }
    }
}

/// Payload for [`TOOL_CALL_AUTH_ERROR`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallAuthErrorPayload {
    pub error_code: Verified,
    pub transport_type: Verified,
    pub auth_error_kind: ToolCallAuthErrorKind,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`ADD`]. The command-line add/import paths share `scope` and
/// `source`; the remaining fields are call-site-specific and therefore
/// optional.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddPayload {
    pub scope: Verified,
    pub source: Verified,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub server_type: Option<Verified>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<Verified>,
    #[serde(rename = "transportExplicit", skip_serializing_if = "Option::is_none")]
    pub transport_explicit: Option<bool>,
    #[serde(rename = "looksLikeUrl", skip_serializing_if = "Option::is_none")]
    pub looks_like_url: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<Verified>,
}

/// Payload for [`DELETE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletePayload {
    pub name: Verified,
    pub scope: Verified,
}

/// Payload for [`GET`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetPayload {
    pub name: Verified,
}

/// Payload for [`COMMAND_INLINE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandInlinePayload {
    pub action: Verified,
}

/// `tengu_mcp_elicitation_*`'s `mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationMode {
    Form,
    Url,
}

impl ElicitationMode {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Url => "url",
        }
    }
}

/// Payload for [`ELICITATION_SHOWN`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationShownPayload {
    pub mode: ElicitationMode,
}

/// Payload for [`ELICITATION_RESPONSE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElicitationResponsePayload {
    pub mode: ElicitationMode,
    pub action: Verified,
}

/// Payload for [`INPUT_MISSING_REQUIRED`]. The oracle spreads additional
/// agent/plugin attribution helpers here; this registry slice models only the
/// exact stable fields the Rust substrate can surface truthfully today.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputMissingRequiredPayload {
    #[serde(rename = "toolName")]
    pub tool_name: Verified,
    #[serde(rename = "isMcp")]
    pub is_mcp: bool,
    #[serde(rename = "toolUseID", skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<Verified>,
    #[serde(rename = "messageID")]
    pub message_id: Verified,
    #[serde(rename = "toolInputSizeBytes")]
    pub tool_input_size_bytes: u64,
    #[serde(rename = "requiredCount")]
    pub required_count: u32,
    #[serde(rename = "missingCount")]
    pub missing_count: u32,
    #[serde(rename = "presentKeyCount")]
    pub present_key_count: u32,
    #[serde(rename = "maxStringValueLen")]
    pub max_string_value_len: u32,
    #[serde(rename = "hasPseudoTagDebris")]
    pub has_pseudo_tag_debris: bool,
    #[serde(rename = "queryChainId", skip_serializing_if = "Option::is_none")]
    pub query_chain_id: Option<Verified>,
    #[serde(rename = "queryDepth", skip_serializing_if = "Option::is_none")]
    pub query_depth: Option<u32>,
    #[serde(rename = "mcpServerType", skip_serializing_if = "Option::is_none")]
    pub mcp_server_type: Option<Verified>,
    #[serde(rename = "mcpServerBaseUrl", skip_serializing_if = "Option::is_none")]
    pub mcp_server_base_url: Option<Verified>,
    #[serde(rename = "requestId", skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Verified>,
}

/// Payload for [`LARGE_RESULT_HANDLED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LargeResultHandledPayload {
    pub outcome: Verified,
    pub reason: Verified,
    pub size_estimate_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persisted_size_chars: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_type: Option<Verified>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persisted_as: Option<Verified>,
}

/// Payload for [`PENDING_CALL`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingCallPayload {
    pub requested_count: u32,
    pub connected_count: u32,
    pub cached_count: u32,
    pub failed_count: u32,
    pub pending_count: u32,
    pub needs_auth_count: u32,
    pub disabled_count: u32,
    pub unconfigured_count: u32,
    pub unknown_count: u32,
    pub wait_ms: u64,
    pub matched: bool,
    pub match_type: Verified,
    pub success: bool,
}

/// Payload for [`SERVERS`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServersPayload {
    pub enterprise: u32,
    pub global: u32,
    pub project: u32,
    pub user: u32,
    pub plugin: u32,
    pub agent: u32,
    pub claudeai: u32,
}

/// Payload for [`TOOL_RESULT_ENDED_TURN`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolResultEndedTurnPayload {
    pub query_chain_id: Verified,
    pub query_depth: u32,
    pub source: Verified,
}

/// Payload for [`TOOLS_COMMANDS_LOADED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsCommandsLoadedPayload {
    pub tools_count: u32,
    pub commands_count: u32,
    pub commands_metadata_length: u32,
}

/// Payload for [`TOOLS_REFRESHED_MID_TURN`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolsRefreshedMidTurnPayload {
    pub old_mcp_count: u32,
    pub new_mcp_count: u32,
    pub recovered: bool,
}

/// Payload for [`OAUTH_FLOW_FAILURE`]. The 2.1.252 oracle emits this only for
/// the XAA branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthFlowFailurePayload {
    pub auth_method: Verified,
    pub xaa_failure_stage: Verified,
    pub id_token_cache_hit: bool,
}

/// XAA-specific shape of [`OAUTH_FLOW_SUCCESS`]. The ordinary interactive
/// OAuth flow uses [`OAuthFlowSuccessPayload`]; Claude Code deliberately emits
/// this smaller `{authMethod,idTokenCacheHit}` shape for silent XAA.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthXaaFlowSuccessPayload {
    /// Literal `xaa` for the silent cross-app flow.
    pub auth_method: Verified,
    /// Whether a reusable IdP token existed before acquisition began.
    pub id_token_cache_hit: bool,
}

/// Payload for [`SESSION_EXPIRED`]. Raw server/tool names are deliberately not
/// modeled: the oracle exposes them only behind a first-party allow gate. The
/// provider-neutral identity is the hashed server key plus a hash of the
/// credential-, query-, and fragment-free base URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionExpiredPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<Verified>,
    pub transport_type: Verified,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server_base_url: Option<Verified>,
    pub mcp_server_key_hash: Verified,
}

/// Payload for [`LIST_PAGINATED`]. `page_count` and `source` are optional
/// because the 2.1.252 oracle has one helper arm that omits them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ListPaginatedPayload {
    pub method: Verified,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    pub item_count: u32,
    pub outcome: Verified,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Verified>,
}

/// Payload for [`RECONCILE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ReconcilePayload {
    pub caller: Verified,
    pub desired_count: u32,
    pub current_count: u32,
    pub to_remove_count: u32,
    pub to_add_count: u32,
    pub to_replace_count: u32,
    pub retained_plugin_count: u32,
}

/// Oracle `HT(name, config)` — decides whether `mcpServerName` is attached
/// to `tengu_mcp_tools_listed` / `tengu_mcp_degraded` at all (see the module
/// doc for the full disassembly). Every arm of `HT` tests for a FIRST-PARTY
/// Anthropic server; for an ordinary user-configured server it is false and
/// the oracle omits the key entirely.
///
/// # This port evaluates to `false` for everything it can currently build
///
/// The gate's four true-arms are, one by one, unreachable here:
///
/// - `dM` (stdio AND `ln(name)` equal to one of two fixed built-in server
///   names, `OH`@155370892 / `F6`@155377771) — this port has neither
///   built-in server, so no configured name can match.
/// - `"url" in t && X7e(t.url) && ln(e)===uy` — same two fixed names.
/// - `$M`'s `type === "claudeai-proxy"` — [`platform_api::McpTransportSpec`] has
///   no `claudeai-proxy` variant (`kind()` yields only `stdio`/`sse`/`http`/
///   `websocket`/`inprocess`/`sse-ide`/`sdk-control`), so no live spec can
///   produce that string.
/// - `$M`'s `tM(url)` / `rA(url)` official-registry and Anthropic
///   `/v1/design/` URL allowlists — not ported.
///
/// Returning a constant `false` is therefore the byte-correct answer for
/// every server this port can reach, and it is safe by construction: the
/// only possible divergence is under-reporting the name of a first-party
/// server that cannot exist here, never leaking a user's private one. Wiring
/// a real predicate is the job of whichever change first ports one of the
/// four arms above; this function is the single place to do it.
#[must_use]
pub fn server_name_gate(transport_kind: &str) -> bool {
    // `$M`'s `e === "claudeai-proxy"` arm, spelled out so that adding the
    // variant to `McpTransportSpec` lights this up without a second search.
    // The remaining three arms need substrate this port does not have.
    transport_kind == "claudeai-proxy"
}

/// Where the invalid-config classification was raised. Oracle:
/// `source:w(t.configError?"loader":"connect")`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfigInvalidSource {
    /// Stamped at config-load time (`mcp/src/json_config.rs`) — a url that
    /// expanded to empty.
    Loader,
    /// Raised at connect time — a syntactically invalid, non-empty url.
    Connect,
}

impl ConfigInvalidSource {
    /// The wire (snake_case) string this variant serializes to — used by
    /// [`crate::emit_mcp_server_config_invalid`], which needs the bare
    /// string for a `tracing` field rather than a `Debug`/JSON round-trip.
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Loader => "loader",
            Self::Connect => "connect",
        }
    }
}

/// Payload for [`SERVER_CONFIG_INVALID`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfigInvalidPayload {
    /// Transport kind (`stdio`/`sse`/`http`/…), defaulted like the oracle's
    /// `c(t.type??"stdio")`.
    pub transport_type: Verified,
    /// The offending config field. Oracle only ever passes the literal
    /// `"url"` here (the sole connect-time re-validation target).
    pub field: Verified,
    /// Loader vs. connect-time classification.
    pub source: ConfigInvalidSource,
}

/// Payload for [`SERVER_CONNECTION_SUCCEEDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConnectionSucceededPayload {
    pub connection_duration_ms: u64,
    pub transport_type: Verified,
    pub scope: Verified,
    pub is_plugin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negotiation_mode: Option<Verified>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_era: Option<Verified>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negotiated_protocol_version: Option<Verified>,
}

/// Payload for [`SERVER_CONNECTION_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConnectionFailedPayload {
    pub transport_type: Verified,
    pub scope: Verified,
    pub is_plugin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negotiation_mode: Option<Verified>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<Verified>,
}

/// Payload for [`TOOLS_LISTED`]. See the module doc for the two fields the
/// byte-alignment audit's guess did NOT hold for (`normalizedCount` /
/// `keptCount`) — intentionally absent here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsListedPayload {
    /// Transport kind, same default rule as [`ServerConfigInvalidPayload`].
    pub transport_type: Verified,
    /// Wall-clock duration of the `tools/list` round-trip in milliseconds.
    pub list_duration_ms: u64,
    /// Number of tools returned.
    pub tool_count: u32,
    /// Number of returned tools flagged `alwaysLoad: true`.
    pub always_load_count: u32,
    /// Discovery source (`live`, cached-row adoption, …).
    pub discovery_source: Verified,
    /// The (possibly disambiguated) server display name — present ONLY when
    /// the oracle's first-party gate holds ([`server_name_gate`]). Absent
    /// for every ordinary user-configured server; see the module doc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server_name: Option<Verified>,
}

/// Payload for [`DISCOVERY_SOURCE`] (§11).
///
/// Two oracle call sites feed this event, both recovered from 2.1.251 in the
/// same chunk as `cot`/`me` (`mcp::discovery_cache`'s module doc has the full
/// disassembly):
///
/// * A fresh/stale HIT (@182535700 region): `s("tengu_mcp_discovery_source",
///   {source:w(U.kind==="fresh"?"cache_fresh":"cache_stale"),
///   transportType:c(E.type??"stdio"),entryAgeMs:ie,...Oe(E,C)})` —
///   unconditional once a `Fresh`/`Stale` decision is reached.
/// * A MISS on the live-dial path (@182538500 region): `if(Ho()&&Ko(U))
///   s("tengu_mcp_discovery_source",{source:w(Jo(U)),
///   transportType:c(E.type??"stdio"),...Oe(E,C)})` — gated by `Ko` (see
///   the `mcp` crate's `discovery_cache::miss_emits_discovery_source_telemetry`,
///   which ports `Ko` byte-exact) and carries NO `entryAgeMs` (there is no
///   entry).
///
/// Both call sites also spread `...Oe(E,C)` — the SAME `mcpServerBaseUrl`/
/// `mcpServerKeyHash` identity spread already documented as known-missing on
/// [`ToolsListedPayload`]'s module doc (`Xe`/`Oe` are chunk-local aliases for
/// one helper); not modelled here for the same reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverySourcePayload {
    /// Transport kind, same default rule as [`ServerConfigInvalidPayload`].
    pub transport_type: Verified,
    /// `cache_fresh` / `cache_stale` (HIT) or one of `mcp`'s
    /// `discovery_cache::miss_telemetry_value` strings (MISS): `live`,
    /// `miss_disabled`, `miss_expired`, `miss_corrupt`, `miss_strike`,
    /// `miss_no_fingerprint`.
    pub source: Verified,
    /// Entry age in milliseconds — present ONLY on a `Fresh`/`Stale` HIT
    /// (oracle `entryAgeMs:ie`). Always absent on a MISS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_age_ms: Option<u64>,
}

/// `tengu_mcp_list_changed`'s `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListChangedType {
    Tools,
    Prompts,
    Resources,
}

impl ListChangedType {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Prompts => "prompts",
            Self::Resources => "resources",
        }
    }
}

/// Payload for [`LIST_CHANGED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListChangedPayload {
    pub kind: ListChangedType,
    /// Privacy-safe stable hash of the configured server key.
    pub mcp_server_key_hash: Verified,
    /// Refresh trigger (`notification` today; the oracle also uses a distinct
    /// listen-reopen cause on the subscription recovery path).
    pub cause: Verified,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_count: Option<u32>,
}

/// Payload for [`RESOURCE_TEMPLATES_FETCHED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceTemplatesFetchedPayload {
    pub template_count: u32,
}

/// `tengu_mcp_listen_reopen`'s `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenReopenOutcome {
    OpenedFromZero,
    Reopened,
    GaveUp,
    BudgetExhausted,
    Parked,
}

impl ListenReopenOutcome {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::OpenedFromZero => "opened_from_zero",
            Self::Reopened => "reopened",
            Self::GaveUp => "gave_up",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Parked => "parked",
        }
    }
}

/// `tengu_mcp_listen_reopen`'s `trigger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenReopenTrigger {
    Connect,
    Remote,
    Graceful,
}

impl ListenReopenTrigger {
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Remote => "remote",
            Self::Graceful => "graceful",
        }
    }
}

/// Payload for [`LISTEN_REOPEN`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenReopenPayload {
    pub mcp_server_key_hash: Verified,
    pub outcome: ListenReopenOutcome,
    pub attempts: u32,
    pub trigger: ListenReopenTrigger,
}

/// `tengu_mcp_degraded`'s `reason`.
///
/// An earlier revision called the eight tool-schema values below "the eight
/// oracle-confirmed values … the complete `yn` set". **That was wrong.**
/// Scanning 2.1.251 for `tengu_mcp_degraded",{reason:w("…")}` yields TEN
/// literal reasons, and two more arrive through the `reason` parameter of
/// `_n` (@182328873), called with `w("resources_list_failed")` and
/// `w("prompts_list_failed")`:
///
/// | reason | oracle site | ported |
/// |---|---|---|
/// | `connected_zero_tools` | `yn` @182316780, its FIRST statement | ✅ |
/// | `tool_schema_normalized` … `tool_property_key_invalid_gated` (7) | `yn` tail | ✅ |
/// | `schema_validator_unavailable` | `qr()` @182172950 | ✅ |
/// | `tools_list_failed` | @182328200 | ✅ |
/// | `resources_list_failed` / `prompts_list_failed` | `_n` @182328873 | ✅ |
///
/// The missed `connected_zero_tools` sat 20 lines ABOVE the seven counters
/// the module doc transcribed verbatim, inside the very function that doc
/// claimed to have traced — which is exactly how a wrong completeness claim
/// hid a gap.
///
/// `#[non_exhaustive]` matches this module's established convention
/// ([`ConfigInvalidSource`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DegradedReason {
    /// The server connected and `tools/list` succeeded but returned ZERO
    /// tools, on a LIVE dial (`r === "live"`; the cached-row adoption paths
    /// do not fire it). Oracle `yn`'s first statement:
    /// `if(u.length===0&&r==="live")s("tengu_mcp_degraded",
    /// {reason:w("connected_zero_tools"),transportType:c(e.config.type
    /// ??"stdio"),mcpServerName:P,..._})`.
    ///
    /// Counted off the RAW `tools/list` response, BEFORE the §20a schema
    /// filter runs — a server whose every tool was dropped by that filter
    /// is NOT `connected_zero_tools`, it is one of the drop reasons below.
    /// Carries none of the count fields.
    ConnectedZeroTools,
    /// `tools/list` failed after the transport initialized. Carries none of
    /// the count fields.
    ToolsListFailed,
    /// `resources/list` failed after the transport initialized. Carries none
    /// of the count fields.
    ResourcesListFailed,
    /// `prompts/list` failed after the transport initialized. Carries none of
    /// the count fields.
    PromptsListFailed,
    /// A root-combinator schema was flattened (the normalize gate was ON).
    /// Carries [`DegradedPayload::normalized_count`].
    ToolSchemaNormalized,
    /// A root-combinator schema WOULD have been flattened but the normalize
    /// gate was OFF, so the tool was dropped instead. Carries
    /// [`DegradedPayload::skipped_count`].
    ToolSchemaNormalizeGated,
    /// A root-combinator value couldn't be represented as a flattened
    /// schema at all (unconditionally dropped, no gate involved). Carries
    /// [`DegradedPayload::skipped_count`].
    ToolSchemaUnsupported,
    /// The schema failed the meta-schema validity check and the drop gate
    /// was ON (tool dropped). Carries [`DegradedPayload::skipped_count`].
    ToolSchemaInvalid,
    /// A top-level property key failed the naming regex and the drop gate
    /// was ON (tool dropped). Carries [`DegradedPayload::skipped_count`].
    ToolPropertyKeyInvalid,
    /// The schema failed the meta-schema validity check but the drop gate
    /// was OFF (tool kept, with a warning). Carries
    /// [`DegradedPayload::kept_count`].
    ToolSchemaInvalidGated,
    /// A top-level property key failed the naming regex but the drop gate
    /// was OFF (tool kept, with a warning). Carries
    /// [`DegradedPayload::kept_count`].
    ToolPropertyKeyInvalidGated,
    /// The bundled JSON-Schema 2020-12 meta-validator failed to compile —
    /// process-global, fires at most once, carries none of the per-server
    /// fields (no `transport_type`/count/`mcp_server_name`).
    SchemaValidatorUnavailable,
}

impl DegradedReason {
    /// The wire (`snake_case`) string this variant serializes to — used by
    /// [`crate::emit_mcp_degraded`], which needs the bare string for a
    /// `tracing` field rather than a JSON round-trip.
    #[must_use]
    pub fn wire_str(self) -> &'static str {
        match self {
            Self::ConnectedZeroTools => "connected_zero_tools",
            Self::ToolsListFailed => "tools_list_failed",
            Self::ResourcesListFailed => "resources_list_failed",
            Self::PromptsListFailed => "prompts_list_failed",
            Self::ToolSchemaNormalized => "tool_schema_normalized",
            Self::ToolSchemaNormalizeGated => "tool_schema_normalize_gated",
            Self::ToolSchemaUnsupported => "tool_schema_unsupported",
            Self::ToolSchemaInvalid => "tool_schema_invalid",
            Self::ToolPropertyKeyInvalid => "tool_property_key_invalid",
            Self::ToolSchemaInvalidGated => "tool_schema_invalid_gated",
            Self::ToolPropertyKeyInvalidGated => "tool_property_key_invalid_gated",
            Self::SchemaValidatorUnavailable => "schema_validator_unavailable",
        }
    }
}

/// Payload for [`DEGRADED`]. Exactly one of `normalized_count` /
/// `skipped_count` / `kept_count` is populated, matching whichever
/// [`DegradedReason`] fired — see each variant's doc. The
/// [`DegradedReason::SchemaValidatorUnavailable`] case populates none of
/// them, nor `transport_type` / `mcp_server_name` (it is not scoped to a
/// server or a tool list).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DegradedPayload {
    /// Which classification fired.
    pub reason: DegradedReason,
    /// Transport kind, same default rule as [`ServerConfigInvalidPayload`].
    /// Absent only for [`DegradedReason::SchemaValidatorUnavailable`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport_type: Option<Verified>,
    /// Count of tools normalized on this server this pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_count: Option<u32>,
    /// Count of tools dropped on this server this pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_count: Option<u32>,
    /// Count of tools kept-with-warning on this server this pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_count: Option<u32>,
    /// The (possibly disambiguated) server display name. Absent only for
    /// [`DegradedReason::SchemaValidatorUnavailable`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server_name: Option<Verified>,
}
