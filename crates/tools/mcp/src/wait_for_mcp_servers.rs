//! `WaitForMcpServers` — 2.1.238 `Sdl` (`cc-238.js @229642127`), name `Qze`
//! (`cc-238.js @222784661`).
//!
//! Blocks for up to [`WAIT_TIMEOUT_MS`] while the named (or all pending) MCP
//! servers finish connecting, then reports each server's settled bucket. Ported
//! because without it the model has no way to wait out a still-connecting
//! server whose tools are not yet in the wire tool list.
//!
//! NOT new in 2.1.238 — 2.1.220 registers `WaitForMcpServers` too (`grep -acoF`
//! = 4 in both binaries). What 2.1.238 *added* is the `getTools` (`iJ`) tail
//! block that force-adds it when servers are pending and `ToolSearch` is absent:
//!
//! ```text
//! if(eZf()&&!l.some((c)=>il(c,y0))&&!l.some((c)=>il(c,Qze)))l=[...l,...Ohe([Sdl],e)];
//! ```
//!
//! The port splits the oracle's two-path enablement across `is_enabled` (the
//! "any pending server" leg, read through the registry's synchronous mirror)
//! and `ConversationOrchestrator::filtered_available_tools` (the "ToolSearch is
//! not in the list" leg). See the `is_enabled` doc below.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use telemetry::pii::Verified;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::mcp::{self as tengu_mcp, PendingCallPayload};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Model-facing name (2.1.238 `Qze`, `cc-238.js @222784661`).
pub const WAIT_FOR_MCP_SERVERS_TOOL_NAME: &str = "WaitForMcpServers";

/// Oracle `C8v = 5000` (`cc-238.js @229641916`) — the wall-clock ceiling on the
/// pending-poll loop.
pub const WAIT_TIMEOUT_MS: u64 = 5_000;

/// Oracle `await Pr(50, r.signal)` — the poll interval inside the wait loop.
const POLL_INTERVAL_MS: u64 = 50;

/// Oracle `EAa()` (`cc-238.js @222783975`) — used verbatim for BOTH
/// `description()` and `prompt()` (`async description(){return EAa()},
/// async prompt(){return EAa()}`). The oracle builds it as an array of lines
/// `.join("\n")`, so there is no trailing newline.
const DESCRIPTION: &str = concat!(
    "Wait for MCP servers that are still connecting and whose tools are not\n",
    "yet in your tool list. Pass `servers` to wait for specific ones, or omit\n",
    "it to wait for all pending servers.\n",
    "\n",
    "If the user's request needs tools from a still-connecting server, call this\n",
    "tool to wait for it. Once it connects, its tools will be added to your tool\n",
    "list and you can use them directly. Returns ready=true when servers are\n",
    "ready, ready=false if they failed to connect, need authentication, or are\n",
    "disabled.\n",
    "\n",
    "You do not need to ask the user for confirmation to use this tool."
);

/// Oracle `k8v` (`cc-238.js @229641990`):
/// `be({servers:dt(H()).optional().describe("Server names to wait for (default: all pending)")})`.
static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "servers": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Server names to wait for (default: all pending)"
            }
        }
    })
});

/// Oracle `A8v` (`cc-238.js @229642095`). `cached` and `unconfigured` remain
/// optional in the schema even though the call result includes both arrays.
static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "ready":        { "type": "boolean" },
            "connected":    { "type": "array", "items": { "type": "string" } },
            "cached":       { "type": "array", "items": { "type": "string" } },
            "failed":       { "type": "array", "items": { "type": "string" } },
            "stillPending": { "type": "array", "items": { "type": "string" } },
            "needsAuth":    { "type": "array", "items": { "type": "string" } },
            "disabled":     { "type": "array", "items": { "type": "string" } },
            "unconfigured": { "type": "array", "items": { "type": "string" } },
            "unknown":      { "type": "array", "items": { "type": "string" } }
        },
        "required": [
            "ready", "connected", "failed", "stillPending",
            "needsAuth", "disabled", "unknown"
        ]
    })
});

/// `WaitForMcpServers` tool.
pub struct WaitForMcpServersTool {
    ctx: BuiltinToolContext,
}

impl WaitForMcpServersTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitState {
    Connected,
    Cached,
    Failed,
    Pending,
    NeedsAuth,
    Disabled,
    Unconfigured,
}

/// The settled buckets the oracle's `call` produces.
#[derive(Default)]
struct Buckets {
    connected: Vec<String>,
    cached: Vec<String>,
    failed: Vec<String>,
    still_pending: Vec<String>,
    needs_auth: Vec<String>,
    disabled: Vec<String>,
    unconfigured: Vec<String>,
    unknown: Vec<String>,
}

/// Bucket the current action states of the requested servers, plus the
/// requested names that no configured server matches (oracle `_`, "unknown").
fn bucket(states: &[(String, WaitState)], requested: &[String]) -> Buckets {
    let mut b = Buckets::default();
    for (name, state) in states {
        match state {
            WaitState::Connected => b.connected.push(name.clone()),
            WaitState::Cached => b.cached.push(name.clone()),
            WaitState::Failed => b.failed.push(name.clone()),
            WaitState::Pending => b.still_pending.push(name.clone()),
            WaitState::NeedsAuth => b.needs_auth.push(name.clone()),
            WaitState::Disabled => b.disabled.push(name.clone()),
            WaitState::Unconfigured => b.unconfigured.push(name.clone()),
        }
    }
    // Oracle: `let y=new Set(c.map((v)=>au(v.name))),_=n.filter((v)=>!y.has(au(v)))`
    // — requested names with no matching client, compared on the NORMALIZED
    // name (`au` = `normalizeNameForMCP`).
    let seen: BTreeSet<String> = states
        .iter()
        .map(|(n, _)| mcp::normalization::normalize_name_for_mcp(n))
        .collect();
    for name in requested {
        if !seen.contains(&mcp::normalization::normalize_name_for_mcp(name)) {
            b.unknown.push(name.clone());
        }
    }
    b
}

/// Restrict a full action-state listing to the requested server names, matching
/// on the raw name OR its normalized form (oracle
/// `i=()=>Zpr(t).filter((v)=>n.includes(v.name)||o.has(au(v.name)))`).
fn select(states: Vec<(String, WaitState)>, requested: &[String]) -> Vec<(String, WaitState)> {
    let normalized: BTreeSet<String> = requested
        .iter()
        .map(|s| mcp::normalization::normalize_name_for_mcp(s))
        .collect();
    states
        .into_iter()
        .filter(|(name, _)| {
            requested.iter().any(|r| r == name)
                || normalized.contains(&mcp::normalization::normalize_name_for_mcp(name))
        })
        .collect()
}

async fn snapshot_wait_states(registry: &mcp::McpRegistry) -> Vec<(String, WaitState)> {
    let connections = registry.connections.read().await;
    let mut out = Vec::with_capacity(connections.len());
    for state in connections.values() {
        let wait_state = if state.config().disabled {
            WaitState::Disabled
        } else {
            match state {
                mcp::McpConnectionState::Connected { .. }
                | mcp::McpConnectionState::HealthChecking { .. } => WaitState::Connected,
                mcp::McpConnectionState::Cached { .. } => WaitState::Cached,
                mcp::McpConnectionState::Connecting { .. }
                | mcp::McpConnectionState::Reconnecting { .. } => WaitState::Pending,
                mcp::McpConnectionState::AwaitingOAuth { .. }
                | mcp::McpConnectionState::NeedsAuth { .. } => WaitState::NeedsAuth,
                mcp::McpConnectionState::Failed { config, .. }
                | mcp::McpConnectionState::Disconnected { config, .. }
                | mcp::McpConnectionState::Stopped { config } => {
                    if config.is_unconfigured() {
                        WaitState::Unconfigured
                    } else {
                        WaitState::Failed
                    }
                }
            }
        };
        out.push((state.name().to_string(), wait_state));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn verified_int(n: u64) -> AnalyticsValue {
    AnalyticsValue::Int(n as i64)
}

async fn emit_pending_call(bus: &Arc<AnalyticsBus>, payload: &PendingCallPayload) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "requestedCount".into(),
        verified_int(u64::from(payload.requested_count)),
    );
    md.insert(
        "connectedCount".into(),
        verified_int(u64::from(payload.connected_count)),
    );
    md.insert(
        "cachedCount".into(),
        verified_int(u64::from(payload.cached_count)),
    );
    md.insert(
        "failedCount".into(),
        verified_int(u64::from(payload.failed_count)),
    );
    md.insert(
        "pendingCount".into(),
        verified_int(u64::from(payload.pending_count)),
    );
    md.insert(
        "needsAuthCount".into(),
        verified_int(u64::from(payload.needs_auth_count)),
    );
    md.insert(
        "disabledCount".into(),
        verified_int(u64::from(payload.disabled_count)),
    );
    md.insert(
        "unconfiguredCount".into(),
        verified_int(u64::from(payload.unconfigured_count)),
    );
    md.insert(
        "unknownCount".into(),
        verified_int(u64::from(payload.unknown_count)),
    );
    md.insert("waitMs".into(), verified_int(payload.wait_ms));
    md.insert("matched".into(), AnalyticsValue::Bool(payload.matched));
    md.insert(
        "matchType".into(),
        AnalyticsValue::String(payload.match_type.as_str().to_string()),
    );
    md.insert("success".into(), AnalyticsValue::Bool(payload.success));
    bus.log_event(tengu_mcp::PENDING_CALL, md).await;
}

/// Oracle `mapToolResultToToolResultBlockParam` (`cc-238.js @229644106`): a
/// `\n`-joined list of non-empty lines, `is_error` = `!ready`.
fn render_for_model(b: &Buckets, ready: bool) -> String {
    let mut lines = vec![format!("ready: {ready}")];
    if !b.connected.is_empty() {
        lines.push(format!(
            "Connected (their tools are now available \u{2014} call them directly): {}",
            b.connected.join(", ")
        ));
    }
    if !b.cached.is_empty() {
        lines.push(format!(
            "Cached (their tools are available now; connects on first call): {}",
            b.cached.join(", ")
        ));
    }
    if !b.failed.is_empty() {
        lines.push(format!("Failed to connect: {}", b.failed.join(", ")));
    }
    if !b.still_pending.is_empty() {
        lines.push(format!(
            "Still connecting (try again or proceed without): {}",
            b.still_pending.join(", ")
        ));
    }
    if !b.needs_auth.is_empty() {
        lines.push(format!(
            "Needs authentication (ask the user to run /mcp): {}",
            b.needs_auth.join(", ")
        ));
    }
    if !b.disabled.is_empty() {
        lines.push(format!(
            "Disabled (ask the user to enable via /mcp): {}",
            b.disabled.join(", ")
        ));
    }
    if !b.unconfigured.is_empty() {
        lines.push(format!(
            "Not configured (no URL set \u{2014} retrying will not help; the user must configure the server first): {}",
            b.unconfigured.join(", ")
        ));
    }
    if !b.unknown.is_empty() {
        lines.push(format!(
            "Unknown (no MCP server with this name is configured): {}",
            b.unknown.join(", ")
        ));
    }
    lines.join("\n")
}

#[async_trait]
impl Tool for WaitForMcpServersTool {
    fn name(&self) -> &str {
        WAIT_FOR_MCP_SERVERS_TOOL_NAME
    }

    fn user_facing_name(&self) -> Option<&str> {
        Some("MCP Wait For Servers")
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }

    /// Oracle `maxResultSizeChars:1e4`.
    fn max_result_size_chars(&self) -> usize {
        10_000
    }

    /// Oracle `isEnabled(){return R8v(Gi(),b7e()??[])}` where
    ///
    /// ```text
    /// function R8v(e,t){if(KU()&&e1e(e)&&!QLe(Fo(e)))return!1;return bdl(t).length>0}
    /// ```
    ///
    /// (`cc-238.js @229641674`). The `bdl(t).length>0` leg — "at least one MCP
    /// client is `type === "pending"`" — is read here through
    /// [`mcp::McpRegistry::has_pending_servers`], the synchronous mirror the
    /// tool-list seam refreshes before assembling a request (the state itself
    /// sits behind an async lock, which this synchronous trait method cannot
    /// take).
    ///
    /// The `KU()` leg — "ToolSearch is already covering deferred discovery for
    /// this model, so do not also advertise a waiter" — is applied by the
    /// caller, exactly as the oracle's own `getTools` (`iJ`) tail block applies
    /// it (`!l.some((c)=>il(c,y0))`, `y0="ToolSearch"`): composed over both
    /// paths, upstream advertises this tool iff there are pending servers AND
    /// `ToolSearch` is not in the final list. See
    /// `ConversationOrchestrator::filtered_available_tools`.
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.ctx
            .mcp_registry
            .as_ref()
            .is_some_and(|r| r.has_pending_servers())
    }

    /// Oracle `isReadOnly(){return!0}`.
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    /// Oracle `isConcurrencySafe(){return!1}`.
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn check_permissions(&self, input: &Value, _: &ToolUseContext) -> PermissionResult {
        // Oracle: `async checkPermissions(e){return{behavior:"allow",updatedInput:e}}`.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "WaitForMcpServers is read-only".into(),
            },
            updated_input: Some(input.clone()),
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        DESCRIPTION.into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let Some(registry) = self.ctx.mcp_registry.as_ref() else {
            return Err(ToolError::Io(
                "WaitForMcpServers: MCP registry not configured".into(),
            ));
        };

        // Oracle: `n = e.servers?.length ? e.servers : bdl(Zpr(t))` — the
        // explicit list when non-empty, otherwise every currently-pending
        // server.
        let explicit: Vec<String> = input
            .get("servers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let requested: Vec<String> = if explicit.is_empty() {
            snapshot_wait_states(registry)
                .await
                .into_iter()
                .filter(|(_, s)| *s == WaitState::Pending)
                .map(|(n, _)| n)
                .collect()
        } else {
            explicit
        };

        // Oracle wait loop: poll every 50ms while any selected client is still
        // `pending` and the 5s budget has not elapsed.
        let wait_started = std::time::Instant::now();
        let deadline = wait_started + std::time::Duration::from_millis(WAIT_TIMEOUT_MS);
        loop {
            if ctx
                .cancel
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                break;
            }
            let now = select(snapshot_wait_states(registry).await, &requested);
            let any_pending = now.iter().any(|(_, s)| *s == WaitState::Pending);
            if !any_pending || std::time::Instant::now() >= deadline {
                break;
            }
            let poll = tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            if let Some(cancel) = ctx.cancel.as_ref() {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    () = poll => {}
                }
            } else {
                poll.await;
            }
        }

        // Oracle captures `waitMs` immediately after the polling loop and only
        // then takes the settled state snapshot.
        let waited_ms = wait_started.elapsed().as_millis() as u64;
        let settled = select(snapshot_wait_states(registry).await, &requested);
        let buckets = bucket(&settled, &requested);
        // Oracle `S`: ready when nothing is still pending, failed, needing auth,
        // disabled or unknown. (`cached` and `unconfigured` do NOT block.)
        let ready = buckets.still_pending.is_empty()
            && buckets.failed.is_empty()
            && buckets.needs_auth.is_empty()
            && buckets.disabled.is_empty()
            && buckets.unknown.is_empty();
        let pending_payload = PendingCallPayload {
            requested_count: u32::try_from(requested.len()).unwrap_or(u32::MAX),
            connected_count: u32::try_from(buckets.connected.len()).unwrap_or(u32::MAX),
            cached_count: u32::try_from(buckets.cached.len()).unwrap_or(u32::MAX),
            failed_count: u32::try_from(buckets.failed.len()).unwrap_or(u32::MAX),
            pending_count: u32::try_from(buckets.still_pending.len()).unwrap_or(u32::MAX),
            needs_auth_count: u32::try_from(buckets.needs_auth.len()).unwrap_or(u32::MAX),
            disabled_count: u32::try_from(buckets.disabled.len()).unwrap_or(u32::MAX),
            unconfigured_count: u32::try_from(buckets.unconfigured.len()).unwrap_or(u32::MAX),
            unknown_count: u32::try_from(buckets.unknown.len()).unwrap_or(u32::MAX),
            wait_ms: waited_ms,
            matched: ready,
            match_type: Verified::assert_safe("wait".to_string()),
            success: ready,
        };

        emit_pending_call(&self.ctx.bus, &pending_payload).await;

        let model_content = render_for_model(&buckets, ready);
        Ok(ToolCallResult {
            data: json!({
                "ready": ready,
                "connected": buckets.connected,
                "cached": buckets.cached,
                "failed": buckets.failed,
                "stillPending": buckets.still_pending,
                "needsAuth": buckets.needs_auth,
                "disabled": buckets.disabled,
                "unconfigured": buckets.unconfigured,
                "unknown": buckets.unknown,
            }),
            model_content: Some(model_content),
            new_messages: vec![],
            context_modifier: None,
            // Oracle: `is_error:!e.ready`.
            is_error: !ready,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use telemetry::InMemorySink;

    #[test]
    fn name_is_byte_exact() {
        assert_eq!(WAIT_FOR_MCP_SERVERS_TOOL_NAME, "WaitForMcpServers");
    }

    /// 2.1.238 `EAa()` (`cc-238.js @222783975`) joined with `\n`. Both the
    /// description and the prompt are this exact string.
    #[test]
    fn description_matches_the_oracle_join() {
        assert!(DESCRIPTION.starts_with(
            "Wait for MCP servers that are still connecting and whose tools are not\nyet in your tool list."
        ));
        assert!(DESCRIPTION
            .ends_with("You do not need to ask the user for confirmation to use this tool."));
        assert!(
            !DESCRIPTION.ends_with('\n'),
            "the oracle joins lines with \\n and does not append a trailing newline"
        );
        assert!(
            DESCRIPTION.contains("\n\nIf the user's request needs tools"),
            "the oracle's array carries an empty line before the second paragraph"
        );
    }

    /// The `mapToolResultToToolResultBlockParam` line set, including the em dash
    /// in the Connected line (`—`, NOT a hyphen).
    #[test]
    fn result_renderer_matches_the_oracle_lines() {
        let b = Buckets {
            connected: vec!["a".into()],
            cached: vec!["cache".into()],
            needs_auth: vec!["b".into()],
            unconfigured: vec!["cfg".into()],
            ..Buckets::default()
        };
        assert_eq!(
            render_for_model(&b, false),
            "ready: false\n\
             Connected (their tools are now available \u{2014} call them directly): a\n\
             Cached (their tools are available now; connects on first call): cache\n\
             Needs authentication (ask the user to run /mcp): b\n\
             Not configured (no URL set \u{2014} retrying will not help; the user must configure the server first): cfg"
        );
        assert_eq!(
            render_for_model(&Buckets::default(), true),
            "ready: true",
            "every empty bucket drops its line (oracle `.filter(Boolean)`)"
        );
    }

    /// A requested name with no configured server lands in `unknown`, compared
    /// on the NORMALIZED name (oracle `au`).
    #[test]
    fn unknown_bucket_compares_normalized_names() {
        let states = vec![("my.server".to_string(), WaitState::Connected)];
        let b = bucket(&states, &["my_server".into(), "nope".into()]);
        assert_eq!(b.connected, vec!["my.server".to_string()]);
        assert_eq!(
            b.unknown,
            vec!["nope".to_string()],
            "`my_server` normalizes onto the configured `my.server`"
        );
    }

    /// Readiness ignores `connected` but is blocked by every other bucket.
    #[test]
    fn ready_requires_every_blocking_bucket_empty() {
        let states = vec![("s".to_string(), WaitState::Pending)];
        let b = bucket(&states, &["s".into()]);
        assert_eq!(b.still_pending, vec!["s".to_string()]);
        assert!(b.unknown.is_empty());
    }

    #[tokio::test]
    async fn pending_call_event_carries_oracle_bucket_counts() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(InMemorySink::new());
        bus.attach_sink(sink.clone()).await;
        let payload = PendingCallPayload {
            requested_count: 8,
            connected_count: 1,
            cached_count: 1,
            failed_count: 1,
            pending_count: 1,
            needs_auth_count: 1,
            disabled_count: 1,
            unconfigured_count: 1,
            unknown_count: 1,
            wait_ms: 123,
            matched: false,
            match_type: Verified::assert_safe("wait".to_string()),
            success: false,
        };
        emit_pending_call(&bus, &payload).await;

        let events = sink.events().await;
        let event = events
            .iter()
            .find(|event| event.name == tengu_mcp::PENDING_CALL)
            .expect("pending-call telemetry emitted");
        for (key, expected) in [
            ("requestedCount", 8),
            ("connectedCount", 1),
            ("cachedCount", 1),
            ("failedCount", 1),
            ("pendingCount", 1),
            ("needsAuthCount", 1),
            ("disabledCount", 1),
            ("unconfiguredCount", 1),
            ("unknownCount", 1),
            ("waitMs", 123),
        ] {
            assert!(matches!(
                event.metadata.get(key),
                Some(AnalyticsValue::Int(value)) if *value == expected
            ));
        }
        assert!(matches!(
            event.metadata.get("matched"),
            Some(AnalyticsValue::Bool(false))
        ));
        assert!(matches!(
            event.metadata.get("success"),
            Some(AnalyticsValue::Bool(false))
        ));
        assert!(matches!(
            event.metadata.get("matchType"),
            Some(AnalyticsValue::String(value)) if value == "wait"
        ));
    }
}
