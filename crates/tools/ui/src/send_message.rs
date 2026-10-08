//! `SendMessageTool` — the main-loop swarm `SendMessage` tool.
//!
//! 1:1 Rust port of claude-code's
//! `src/tools/SendMessageTool/SendMessageTool.ts` (name constant
//! `SEND_MESSAGE_TOOL_NAME = "SendMessage"`). It sends a message to a teammate
//! in an agent swarm: a plain-text note (with a short `summary` preview), a
//! broadcast to every teammate (`to: "*"`), or one of three structured
//! protocol messages (`shutdown_request` / `shutdown_response` /
//! `plan_approval_response`).
//!
//! **Schema/validation parity** is reproduced byte-faithfully from the TS
//! (`inputSchema` + `validateInput`). The schema + the structured-message
//! variants mirror the coordinator-side port at
//! `coordinator/src/tool_send_message.rs`, adapted so `parse_recipient`
//! resolves a teammate *name* (the TS contract) rather than a bare UUID.
//!
//! Delivery routes through the injected [`lingxi_core::host::mailbox::MailboxRouterHandle`]
//! seam for teammates. Canonical `session:<uuid>` recipients use the local live
//! session registry with UDS first and a JSONL inbox fallback.
//!
//! The LingXi-internal claim window stays byte-locked at
//! `Duration::from_secs(30)` (spec §7 line 498), and the telemetry event names
//! are unchanged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use lingxi_core::host::mailbox::{MailboxMessage, MailboxRouterHandle};
use lingxi_core::host::task_registry::TaskRegistryHandle;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use telemetry::pii::Verified;
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{SEND_MESSAGE_COMPLETED, SEND_MESSAGE_FAILED, SEND_MESSAGE_STARTED};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};
use tool_api::BuiltinToolContext;

/// Tool name `'SendMessage'` (claude-code `SEND_MESSAGE_TOOL_NAME`).
pub const SEND_MESSAGE_TOOL_NAME: &str = "SendMessage";

/// LingXi-internal claim-window lock — see spec §7 line 498.
/// Encoded as `Duration::from_secs(30)` byte-for-byte in source.
pub const SEND_MESSAGE_CLAIM_WINDOW: Duration = Duration::from_secs(30);

/// `TEAM_LEAD_NAME` — 1:1 with claude-code
/// `src/utils/swarm/constants.ts` (`export const TEAM_LEAD_NAME = 'team-lead'`).
const TEAM_LEAD_NAME: &str = "team-lead";

/// `DESCRIPTION` — 1:1 with `src/tools/SendMessageTool/prompt.ts`.
const DESCRIPTION: &str = "Send a message to another agent";

/// `maxResultSizeChars` in the TS tool is `100_000`.
const MAX_RESULT_SIZE_CHARS: usize = 100_000;

/// Preview width for the `routing.content` field (claude-code `Us(t, 50)`).
///
/// 2.1.212 stopped duplicating the full message body in the sender's transcript:
/// the body rides the mailbox delivery, and the tool result only carries a
/// 50-char preview. See [`truncate_preview`].
const ROUTING_CONTENT_PREVIEW_CHARS: usize = 50;

/// Truncate `s` to `max_width` columns, appending `…` if it was longer.
///
/// Port of claude-code `utils/truncate.ts` `truncate(str, maxWidth)` (the
/// two-arg / non-single-line form, `Us(t, 50)`): return the string unchanged
/// when it fits, otherwise take complete grapheme clusters through
/// `max_width - 1` terminal cells and append the ellipsis. Reuse the TUI's
/// shared width primitive so CJK and multi-codepoint emoji follow the same
/// display-width contract everywhere.
pub fn truncate_preview(s: &str, max_width: usize) -> String {
    tool_api::util::output_truncation::truncate_to_width_ellipsis(s, max_width)
}

/// `isAgentSwarmsEnabled()` gate: Anthropic-internal runs are on by default,
/// while external runs require the experimental env opt-in. `SendMessage`
/// mirrors that runtime gate and additionally requires a live mailbox router.
/// Delegates to the SHARED [`lingxi_core::host::env::agent_swarms_enabled`] (one
/// implementation with `tool-task`'s `is_agent_swarms_enabled`).
fn agent_swarms_enabled() -> bool {
    lingxi_core::host::env::agent_swarms_enabled()
}

/// Resolved `to` recipient.
///
/// `parse_recipient` resolves by teammate **name** first, then the explicit
/// raw agent ID form — mirroring the TS `call` resolution order
/// (`agentNameRegistry.get(to) ?? toAgentId(to)`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recipient {
    /// `to == "*"` — broadcast to every teammate.
    Broadcast,
    /// A teammate addressed by name (the primary path).
    Teammate(String),
    /// A raw host agent ID.
    Agent(String),
    /// A live Claude session addressed by its canonical UUID.
    Session(String),
}

impl Recipient {
    /// String handed to the mailbox seam for a single (non-broadcast) route.
    fn route_target(&self) -> &str {
        match self {
            Recipient::Broadcast => "*",
            Recipient::Teammate(s) | Recipient::Agent(s) | Recipient::Session(s) => s,
        }
    }
}

/// `SendMessageTool` — routes a teammate message via the swarm mailbox seam.
pub struct SendMessageTool {
    ctx: BuiltinToolContext,
}

impl SendMessageTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    fn fresh_invocation_id() -> String {
        tool_api::util::ids::ulid_or_uuid()
    }

    /// Boolean that also accepts the string literals `"true"`/`"false"` — a
    /// port of the TS `semanticBoolean()` used for the `approve` field.
    fn semantic_bool(v: &Value) -> Option<bool> {
        match v {
            Value::Bool(b) => Some(*b),
            Value::String(s) if s == "true" => Some(true),
            Value::String(s) if s == "false" => Some(false),
            _ => None,
        }
    }

    /// Parse the `to` field into a [`Recipient`].
    ///
    /// Resolves by teammate **name** first (any bare string), then the
    /// explicit raw agent ID form, and recognises `"*"` as a
    /// broadcast. Rejection strings are byte-faithful with the TS
    /// `validateInput`.
    ///
    /// # Errors
    /// Returns [`ToolError::InvalidInput`] when `to` is empty or contains `@`
    /// (one team per session — bare name / `*` only in the TS contract).
    fn parse_recipient(to: &str) -> Result<Recipient, ToolError> {
        let trimmed = to.trim();
        if trimmed.is_empty() {
            return Err(ToolError::InvalidInput("to must not be empty".into()));
        }
        if trimmed.contains('@') {
            return Err(ToolError::InvalidInput(
                "to must be a bare teammate name — there is only one team per session".into(),
            ));
        }
        if trimmed == "*" {
            return Ok(Recipient::Broadcast);
        }
        // Resolve by teammate NAME first, then the explicit session/agent UUID
        // forms. The session ID is the canonical cross-process identity.
        if let Some(session_id) = trimmed.strip_prefix("session:") {
            let parsed =
                lingxi_core::types::SessionId::parse_prefixed(session_id).ok_or_else(|| {
                    ToolError::InvalidInput("session address must contain a valid UUID".into())
                })?;
            return Ok(Recipient::Session(parsed.as_uuid().to_string()));
        }
        if !trimmed.contains(':') {
            if let Some(agent) = lingxi_core::types::AgentId::parse_prefixed(trimmed) {
                return Ok(Recipient::Agent(agent.as_uuid().to_string()));
            }
        }
        Ok(Recipient::Teammate(trimmed.to_string()))
    }

    /// Sender display name for `routing.sender`.
    ///
    /// Uses the human teammate name threaded on [`ToolUseContext`], falling to
    /// the same `"teammate"` / `"team-lead"` labels as Claude when no display
    /// name is available.
    fn sender_name(ctx: &ToolUseContext) -> String {
        ctx.agent_name.clone().unwrap_or_else(|| {
            if ctx.agent_id.is_some() {
                "teammate".to_string()
            } else {
                TEAM_LEAD_NAME.to_string()
            }
        })
    }

    /// `from` string handed to the mailbox seam.
    ///
    /// Prefer the display name because the mailbox router and Claude's mailbox
    /// both key teammate attribution on it. An unnamed agent falls back to its
    /// id so the concrete router can still retain teammate attribution.
    fn route_from(ctx: &ToolUseContext) -> String {
        ctx.agent_name.clone().unwrap_or_else(|| {
            ctx.agent_id
                .map_or_else(|| TEAM_LEAD_NAME.to_string(), |a| a.to_string())
        })
    }

    fn idle_subscription_success(target: &str) -> String {
        format!(
            "Subscribed — you will get one notice here when \"{target}\" is next idle (or exits). Do not poll or wait for it; carry on."
        )
    }

    fn self_target_error(
        recipient: &Recipient,
        to_display: &str,
        ctx: &ToolUseContext,
    ) -> Option<String> {
        match recipient {
            Recipient::Agent(id) => ctx.agent_id.and_then(|self_id| {
                (self_id.as_uuid().to_string() == *id).then(|| {
                    format!(
                        "'{to_display}' is this session's own address — a message or file sent there would only come back to this conversation; there is no one else at that address to send to."
                    )
                })
            }),
            Recipient::Session(id) => lingxi_core::host::live_sessions::process_session_id()
                .filter(|self_id| self_id == id)
                .map(|_| format!("'{to_display}' is this session's own address.")),
            Recipient::Teammate(_) => {
                let trimmed = to_display.trim();
                if ctx.agent_name.as_deref().is_some_and(|name| name == trimmed) {
                    return Some(format!("Not sent — '{trimmed}' is this session's own name."));
                }
                if lingxi_core::host::live_sessions::process_name()
                    .as_deref()
                    .is_some_and(|name| name == trimmed)
                {
                    return Some(format!("Not sent — '{trimmed}' is this session's own name."));
                }
                lingxi_core::host::live_sessions::process_dir()
                    .and_then(|dir| dir.find_exact(trimmed, None))
                    .zip(lingxi_core::host::live_sessions::process_session_id())
                    .and_then(|(rec, self_id)| {
                        (rec.sid() == self_id).then(|| {
                            format!(
                                "'{trimmed}' is this session's own address — a message or file sent there would only come back to this conversation; there is no one else at that address to send to."
                            )
                        })
                    })
            }
            Recipient::Broadcast => None,
        }
    }

    /// `generateRequestId('{request_type}', '{agent_id}')` from the TS —
    /// `"{request_type}-{millis}@{agent_id}"`.
    fn generate_request_id(request_type: &str, agent_id: &str) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{request_type}-{millis}@{agent_id}")
    }

    fn protocol_timestamp() -> String {
        tool_api::send_message_contract::protocol_timestamp(SystemTime::now())
    }

    fn require_team_lead(ctx: &ToolUseContext, action: &str) -> Result<(), ToolError> {
        if ctx.agent_id.is_none() && ctx.agent_name.is_none() {
            return Ok(());
        }
        Err(ToolError::InvalidInput(format!(
            "Only the team lead can {action} plans. Teammates cannot {action} their own or other plans."
        )))
    }

    fn approval_permission_mode(&self) -> &'static str {
        let raw = self
            .ctx
            .permission_gate
            .as_ref()
            .and_then(|gate| gate.permission_mode())
            .unwrap_or_else(|| self.ctx.permission_mode.wire_str().to_string());
        let mode = permission::permission_mode_from_cli_string(&raw);
        if mode == permission::PermissionMode::Plan {
            permission::PermissionMode::Default.wire_str()
        } else {
            mode.wire_str()
        }
    }

    /// Refuse a message aimed at an agent the USER stopped (AGT-07).
    ///
    /// Best-effort by construction: no registry wired, an id that does not
    /// resolve, or a lookup error all fall through to the ordinary delivery
    /// path. The gate exists to convert a KNOWN cancellation into something the
    /// model can act on, not to add a new way for `SendMessage` to fail.
    async fn refuse_if_stopped_by_user(
        registry: Option<&Arc<dyn TaskRegistryHandle>>,
        target: &str,
    ) -> Result<(), ToolError> {
        let Some(registry) = registry else {
            return Ok(());
        };
        let Ok(Some(record)) = registry.get(target).await else {
            return Ok(());
        };
        if record.killed_by.as_deref() == Some("user") {
            return Err(ToolError::InvalidInput(
                lingxi_core::host::task_registry::stopped_by_user_message(&record.task_id),
            ));
        }
        Ok(())
    }

    /// Deliver through the live mailbox seam. Delivery failures are surfaced;
    /// returning a success result after `NotFound`/`Full` would falsely tell the
    /// model that a background agent received a course correction.
    async fn deliver(
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        target: &str,
        content: String,
    ) -> Result<String, ToolError> {
        let message_id = tool_api::util::ids::ulid_or_uuid();
        let msg = MailboxMessage {
            message_id: message_id.clone(),
            content,
            timestamp: SystemTime::now(),
            color: if from == TEAM_LEAD_NAME {
                None
            } else {
                router.teammate_color(from).await
            },
        };
        router
            .route(from, target, msg)
            .await
            .map(|_| message_id)
            .map_err(|e| {
                ToolError::Internal(format!("SendMessage: failed to deliver to '{target}': {e}"))
            })
    }

    fn routing(sender: &str, target: &str, summary: Option<&str>, content: Option<&str>) -> Value {
        tool_api::send_message_contract::routing(sender, None, target, None, summary, content)
    }

    async fn teammate_routing(
        router: &Arc<dyn MailboxRouterHandle>,
        sender: &str,
        target: &str,
        summary: Option<&str>,
        content: Option<&str>,
    ) -> Value {
        let sender_color = if sender == TEAM_LEAD_NAME {
            None
        } else {
            router.teammate_color(sender).await
        };
        let target_color = router.teammate_color(target.trim_start_matches('@')).await;
        tool_api::send_message_contract::routing(
            sender,
            sender_color.as_deref(),
            target,
            target_color.as_deref(),
            summary,
            content,
        )
    }

    /// `handleMessage` — a plain-text note to a single teammate.
    ///
    /// The full body rides the mailbox delivery ([`Self::deliver`]); the tool
    /// result only carries a 50-char `routing.content` preview
    /// (claude-code 2.1.212 `content: Us(t, 50)`), so the body is not duplicated
    /// in the sender's transcript.
    async fn handle_message(
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        recipient: &Recipient,
        to_display: &str,
        content: &str,
        summary: Option<&str>,
        sender: &str,
        notify_when_idle: bool,
        task_registry: Option<&Arc<dyn TaskRegistryHandle>>,
    ) -> Result<Value, ToolError> {
        // A teammate name belongs to the current team before it is considered
        // a live-session alias. The router's named-recipient snapshot tells us
        // whether the name is actually owned by the current team; otherwise
        // the compatibility live-session alias may handle it.
        if let Recipient::Teammate(target) = recipient {
            // AGT-07 pre-flight. `deliver` returns as soon as the MAILBOX
            // accepts the message, so the registry's own refusal surfaces a hop
            // later inside the pump and can never reach this tool's return
            // value. Asking first is what makes the user's cancellation legible
            // to the model SYNCHRONOUSLY, in the result of the call it made.
            Self::refuse_if_stopped_by_user(task_registry, target).await?;
            let is_team_recipient = router
                .named_recipients()
                .await
                .into_iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(target));
            if is_team_recipient {
                let message_id = Self::deliver(router, from, target, content.to_string()).await?;
                let preview = truncate_preview(content, ROUTING_CONTENT_PREVIEW_CHARS);
                return Ok(json!({
                    "success": true,
                    "message": format!("Message sent to {to_display}'s inbox"),
                    "msg_id": message_id,
                    "routing": Self::teammate_routing(router, sender, &format!("@{to_display}"), summary, Some(&preview)).await,
                }));
            }
        }

        let live_target = match recipient {
            Recipient::Session(session_id) => lingxi_core::host::live_sessions::process_dir()
                .and_then(|dir| {
                    dir.find_by_session_id(
                        session_id,
                        lingxi_core::host::live_sessions::process_session_id().as_deref(),
                    )
                    .map(|peer| (dir, peer))
                }),
            Recipient::Teammate(_) => {
                lingxi_core::host::live_sessions::process_dir().and_then(|dir| {
                    dir.find_exact(
                        to_display,
                        lingxi_core::host::live_sessions::process_session_id().as_deref(),
                    )
                    .map(|peer| (dir, peer))
                })
            }
            _ => None,
        };
        if let Some((dir, peer)) = live_target {
            let from_name = lingxi_core::host::live_sessions::process_name()
                .unwrap_or_else(|| from.to_string());
            let from_sid =
                lingxi_core::host::live_sessions::process_session_id().unwrap_or_default();
            let preview = truncate_preview(content, ROUTING_CONTENT_PREVIEW_CHARS);
            let message = lingxi_core::host::live_sessions::outbound_peer_message(
                &from_name, &from_sid, content, summary,
            );
            // See `coordinator::tool_send_message::route_live_session` for the
            // two-transport contract: the inbox is the delivery of record and
            // is keyed by the session discovery resolved; the socket is the
            // best-effort wake-up that an idle peer actually hears. Both
            // copies carry one `msg_id`, so the peer sees the message once.
            dir.send_inbox(peer.sid(), &message).map_err(|e| {
                ToolError::Internal(format!(
                    "SendMessage: failed to deliver to live session inbox: {e}"
                ))
            })?;
            if let Some(sock) = peer
                .messaging_socket_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .filter(|p| lingxi_core::host::uds_inbox::is_canonical_inbox_sock(p))
            {
                let _ =
                    lingxi_core::host::uds_inbox::send_peer_message(&sock, &message, peer.sid());
            }
            let subscribed = if notify_when_idle {
                dir.append_idle_subscription(
                    peer.sid(),
                    &lingxi_core::host::live_sessions::IdleNotificationRequest {
                        from: from_name.clone(),
                        from_session_id: from_sid.clone(),
                        summary: summary.map(str::to_string),
                    },
                )
                .map_err(|e| {
                    ToolError::Internal(format!(
                        "SendMessage: failed to register idle notification for live session: {e}"
                    ))
                })?;
                true
            } else {
                false
            };
            return Ok(json!({
                "success": true,
                "message": if subscribed {
                    Self::idle_subscription_success(to_display)
                } else {
                    format!("Message sent to {to_display}'s inbox")
                },
                "routing": Self::routing(
                    sender,
                    &format!("@{to_display}"),
                    summary,
                    Some(&preview),
                ),
            }));
        }
        if matches!(recipient, Recipient::Session(_)) {
            return Err(ToolError::InvalidInput(
                "SendMessage: no live session with that session id".into(),
            ));
        }
        if notify_when_idle {
            return Err(ToolError::InvalidInput(
                "notify_when_idle is only supported for local live sessions".into(),
            ));
        }
        let message_id =
            Self::deliver(router, from, recipient.route_target(), content.to_string()).await?;
        let preview = truncate_preview(content, ROUTING_CONTENT_PREVIEW_CHARS);
        Ok(json!({
            "success": true,
            "message": format!("Message sent to {to_display}'s inbox"),
            "msg_id": message_id,
            "routing": Self::teammate_routing(
                router, sender,
                &format!("@{to_display}"),
                summary,
                Some(&preview),
            ).await,
        }))
    }

    async fn handle_idle_subscription_only(
        recipient: &Recipient,
        to_display: &str,
        summary: Option<&str>,
        sender: &str,
    ) -> Result<Value, ToolError> {
        let Some((dir, peer)) = lingxi_core::host::live_sessions::process_dir().and_then(|d| {
            let peer = match recipient {
                Recipient::Session(session_id) => d.find_by_session_id(
                    session_id,
                    lingxi_core::host::live_sessions::process_session_id().as_deref(),
                ),
                Recipient::Teammate(_) => d.find_exact(
                    to_display,
                    lingxi_core::host::live_sessions::process_session_id().as_deref(),
                ),
                _ => None,
            };
            peer.map(|peer| (d, peer))
        }) else {
            return Err(ToolError::InvalidInput(
                "notify_when_idle is only supported for local live sessions".into(),
            ));
        };
        let from_name =
            lingxi_core::host::live_sessions::process_name().unwrap_or_else(|| sender.to_string());
        let from_sid = lingxi_core::host::live_sessions::process_session_id().unwrap_or_default();
        dir.append_idle_subscription(
            peer.sid(),
            &lingxi_core::host::live_sessions::IdleNotificationRequest {
                from: from_name,
                from_session_id: from_sid,
                summary: summary.map(str::to_string),
            },
        )
        .map_err(|e| {
            ToolError::Internal(format!(
                "SendMessage: failed to register idle notification for live session: {e}"
            ))
        })?;
        Ok(json!({
            "success": true,
            "message": Self::idle_subscription_success(to_display),
            "routing": Self::routing(sender, &format!("@{to_display}"), summary, None),
        }))
    }

    /// `handleBroadcast` — fan-out to every live teammate except the sender.
    async fn handle_broadcast(
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        content: &str,
    ) -> Result<Value, ToolError> {
        let recipients = router
            .broadcast(
                from,
                MailboxMessage {
                    message_id: tool_api::util::ids::ulid_or_uuid(),
                    content: content.to_string(),
                    timestamp: SystemTime::now(),
                    color: None,
                },
            )
            .await
            .map_err(|e| ToolError::Internal(format!("SendMessage: failed to broadcast: {e}")))?;
        let message = if recipients.is_empty() {
            "No teammates to broadcast to (you are the only team member)".to_string()
        } else {
            format!(
                "Message broadcast to {} teammate(s): {}",
                recipients.len(),
                recipients.join(", ")
            )
        };
        Ok(json!({
            "success": true,
            "message": message,
            "recipients": recipients,
        }))
    }

    /// `handleShutdownRequest` — ask a teammate to shut down.
    async fn handle_shutdown_request(
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        recipient: &Recipient,
        to_display: &str,
        reason: Option<&str>,
        sender: &str,
    ) -> Result<Value, ToolError> {
        let request_id = Self::generate_request_id("shutdown", to_display);
        let payload = tool_api::send_message_contract::shutdown_request(
            &request_id,
            sender,
            reason,
            &Self::protocol_timestamp(),
        );
        Self::deliver(
            router,
            from,
            recipient.route_target(),
            serde_json::to_string(&payload).unwrap_or_default(),
        )
        .await?;
        Ok(json!({
            "success": true,
            "message": format!("Shutdown request sent to {to_display}. Request ID: {request_id}"),
            "request_id": request_id,
            "target": to_display,
        }))
    }

    /// `handleShutdownApproval` — a teammate approves its own shutdown.
    ///
    async fn handle_shutdown_approval(
        &self,
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        request_id: &str,
        agent_name: &str,
        ctx: &ToolUseContext,
    ) -> Result<Value, ToolError> {
        let payload = tool_api::send_message_contract::shutdown_approved(
            request_id,
            agent_name,
            &Self::protocol_timestamp(),
            None,
            None,
        );
        Self::deliver(
            router,
            from,
            TEAM_LEAD_NAME,
            serde_json::to_string(&payload).unwrap_or_default(),
        )
        .await?;
        let task_address = ctx
            .agent_id
            .map(|id| id.to_string())
            .or_else(|| ctx.agent_name.clone())
            .ok_or_else(|| {
                ToolError::Internal(
                    "SendMessage: shutdown approval requires teammate context".into(),
                )
            })?;
        let registry = self.ctx.task_registry.as_ref().ok_or_else(|| {
            ToolError::Internal("SendMessage: task registry is not configured".into())
        })?;
        // The team LEAD (an agent) approved this shutdown, so the stop is
        // attributed to Claude, not the user (claude `killedBy:"parent"`).
        registry
            .kill_with_reason(&task_address, "parent")
            .await
            .map_err(|e| ToolError::Internal(format!("SendMessage: shutdown failed: {e}")))?;
        Ok(json!({
            "success": true,
            "message": format!(
                "Shutdown approved. Sent confirmation to team-lead. Agent {agent_name} is now exiting."
            ),
            "request_id": request_id,
        }))
    }

    /// `handleShutdownRejection` — a teammate declines a shutdown request.
    async fn handle_shutdown_rejection(
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        request_id: &str,
        reason: &str,
        agent_name: &str,
    ) -> Result<Value, ToolError> {
        let payload = tool_api::send_message_contract::shutdown_rejected(
            request_id,
            agent_name,
            reason,
            &Self::protocol_timestamp(),
        );
        Self::deliver(
            router,
            from,
            TEAM_LEAD_NAME,
            serde_json::to_string(&payload).unwrap_or_default(),
        )
        .await?;
        Ok(json!({
            "success": true,
            "message": format!("Shutdown rejected. Reason: \"{reason}\". Continuing to work."),
            "request_id": request_id,
        }))
    }

    /// `handlePlanApproval` — the team lead approves a teammate's plan.
    async fn handle_plan_approval(
        &self,
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        recipient: &Recipient,
        to_display: &str,
        request_id: &str,
        feedback: Option<&str>,
        ctx: &ToolUseContext,
    ) -> Result<Value, ToolError> {
        Self::require_team_lead(ctx, "approve")?;
        let mut payload = json!({
            "type": "plan_approval_response",
            "requestId": request_id,
            "approved": true,
            "timestamp": Self::protocol_timestamp(),
            "permissionMode": self.approval_permission_mode(),
        });
        if let Some(feedback) = feedback {
            payload["feedback"] = json!(feedback);
        }
        Self::deliver(
            router,
            from,
            recipient.route_target(),
            serde_json::to_string(&payload).unwrap_or_default(),
        )
        .await?;
        Ok(json!({
            "success": true,
            "message": format!(
                "Plan approved for {to_display}. They will receive the approval and can proceed with implementation."
            ),
            "request_id": request_id,
        }))
    }

    /// `handlePlanRejection` — the team lead rejects a teammate's plan.
    async fn handle_plan_rejection(
        &self,
        router: &Arc<dyn MailboxRouterHandle>,
        from: &str,
        recipient: &Recipient,
        to_display: &str,
        request_id: &str,
        feedback: &str,
        ctx: &ToolUseContext,
    ) -> Result<Value, ToolError> {
        Self::require_team_lead(ctx, "reject")?;
        let payload = json!({
            "type": "plan_approval_response",
            "requestId": request_id,
            "approved": false,
            "feedback": feedback,
            "timestamp": Self::protocol_timestamp(),
        });
        Self::deliver(
            router,
            from,
            recipient.route_target(),
            serde_json::to_string(&payload).unwrap_or_default(),
        )
        .await?;
        let feedback_preview = truncate_preview(feedback, ROUTING_CONTENT_PREVIEW_CHARS);
        Ok(json!({
            "success": true,
            "message": format!(
                "Plan rejected for {to_display} with feedback: \"{feedback_preview}\""
            ),
            "request_id": request_id,
        }))
    }

    async fn emit_started(bus: &Arc<AnalyticsBus>, invocation_id: &str, message_chars: i64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert("message_chars".into(), AnalyticsValue::Int(message_chars));
        bus.log_event(SEND_MESSAGE_STARTED, md).await;
    }

    async fn emit_completed(bus: &Arc<AnalyticsBus>, invocation_id: &str, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".into(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        bus.log_event(SEND_MESSAGE_COMPLETED, md).await;
    }

    async fn emit_failed(
        bus: &Arc<AnalyticsBus>,
        invocation_id: &str,
        error_kind: &str,
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
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        bus.log_event(SEND_MESSAGE_FAILED, md).await;
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        SEND_MESSAGE_TOOL_NAME
    }
    fn search_hint(&self) -> Option<&str> {
        Some("send messages to agent teammates")
    }
    fn input_schema(&self) -> &Value {
        tool_api::send_message_contract::schema(
            lingxi_core::host::live_sessions::cross_session_messaging_enabled(),
            agent_swarms_enabled(),
        )
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // Ordinary named/background agents can receive messages without teams.
        self.ctx.mailbox_router.is_some()
    }
    fn coerce_input(&self, input: &Value) -> Option<tool_api::tool_trait::CoercedInput> {
        tool_api::send_message_contract::coerce(input)
    }
    fn should_defer(&self) -> bool {
        // TS `shouldDefer: true`.
        true
    }
    // no-truncation: SendMessage opts out of the standard MAX_TOOL_OUTPUT_LENGTH
    // (30_000) cap in favor of its own `maxResultSizeChars: 100_000`
    // (claude-code SendMessageTool.ts:524); its results are bounded status
    // strings well under that ceiling.
    fn max_result_size_chars(&self) -> usize {
        MAX_RESULT_SIZE_CHARS
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, input: &Value) -> bool {
        // TS `isReadOnly(input)` = `typeof input.message === 'string'`.
        input.get("message").is_some_and(Value::is_string)
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        // The TS tool only escalates to `ask` for a cross-machine `bridge:`
        // scheme. This tool exposes teammate and canonical local-session sends,
        // both of which remain local and are allowed here.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "SendMessage routes teammate-to-teammate text via the swarm mailbox".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn validate_input(
        &self,
        input: &Value,
        ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        tool_api::send_message_contract::validate(
            input,
            agent_swarms_enabled(),
            ctx.agent_id.is_some() || ctx.agent_name.is_some(),
        )
        .map_err(ValidationError)
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        tool_api::send_message_contract::prompt(
            lingxi_core::host::live_sessions::cross_session_messaging_enabled(),
            agent_swarms_enabled(),
        )
        .into()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let invocation_id = Self::fresh_invocation_id();
        let bus = self.ctx.bus.clone();

        // 2.1.266 `a0`: refuse while THIS agent's own stop is still completing
        // (see `agent_processes::mark_stop_pending`).
        if let Some(agent_id) = ctx.agent_id {
            if lingxi_core::host::agent_processes::is_stop_pending(&agent_id.to_string()) {
                return Err(ToolError::InvalidInput(
                    lingxi_core::host::agent_processes::stop_pending_refusal("send messages."),
                ));
            }
        }

        let input = tool_api::send_message_contract::coerce(&input)
            .map_or(input.clone(), |coerced| coerced.input);
        if input.get("to").and_then(Value::as_str) == Some("*") {
            return Err(ToolError::InvalidInput(
                "broadcast (to: \"*\") is no longer supported — send a message per recipient"
                    .into(),
            ));
        }

        tool_api::send_message_contract::validate(
            &input,
            true,
            ctx.agent_id.is_some() || ctx.agent_name.is_some(),
        )
        .map_err(ToolError::InvalidInput)?;

        // --- Mailbox-seam guard (Rust-port artifact) -------------------------
        // PARITY-GAP: claude-code appends to a file mailbox directly; the Rust
        // port delivers through an injected `MailboxRouterHandle`. With no
        // router wired the tool cannot route at all, so surface a clear internal
        // error before any other work. (Checked first so an unwired host hits
        // this path regardless of payload.)
        let router = match self.ctx.mailbox_router.clone() {
            Some(r) => r,
            None => {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "router_not_wired",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::Internal(
                    "SendMessageTool: MailboxRouterHandle not wired into BuiltinToolContext".into(),
                ));
            }
        };

        // The latest protocol only accepts `to`.
        let to = match input.get("to").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "missing_to",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput("SendMessage: missing 'to'".into()));
            }
        };
        let recipient = match Self::parse_recipient(&to) {
            Ok(r) => r,
            Err(e) => {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "invalid_recipient",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(e);
            }
        };

        // --- Parse `message` (plain string or structured object) -------------
        let message = input.get("message");

        let summary = input
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_string);
        let notify_when_idle = input
            .get("notify_when_idle")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let from = Self::route_from(&ctx);
        let sender = Self::sender_name(&ctx);

        let main_message =
            to.trim().eq_ignore_ascii_case("main") && message.is_some_and(Value::is_string);
        let self_error = if main_message {
            None
        } else {
            Self::self_target_error(&recipient, &to, &ctx)
        };
        if let Some(message) = self_error {
            Self::emit_failed(
                &bus,
                &invocation_id,
                "self_target",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Err(ToolError::InvalidInput(message));
        }

        let message_chars = match message {
            None => 0,
            Some(Value::String(s)) => s.chars().count(),
            Some(other) => serde_json::to_string(other)
                .map(|s| s.chars().count())
                .unwrap_or(0),
        } as i64;
        Self::emit_started(&bus, &invocation_id, message_chars).await;

        let data_result: Result<Value, ToolError> = match message {
            None => {
                if notify_when_idle {
                    Self::handle_idle_subscription_only(
                        &recipient,
                        &to,
                        summary.as_deref(),
                        &sender,
                    )
                    .await
                } else {
                    Self::emit_failed(
                        &bus,
                        &invocation_id,
                        "missing_message",
                        started.elapsed().as_millis() as u64,
                    )
                    .await;
                    Err(ToolError::InvalidInput(
                        "SendMessage: missing 'message'".into(),
                    ))
                }
            }
            Some(Value::String(content)) if main_message => {
                if ctx.agent_id.is_none() {
                    Ok(
                        json!({"success":false,"message":"You are the main conversation — \"main\" addresses you. Send to a named agent instead."}),
                    )
                } else {
                    Self::deliver(&router, &from, "main", content.clone()).await?;
                    Ok(
                        json!({"success":true,"message":"Message queued for the main conversation's next turn."}),
                    )
                }
            }
            Some(Value::String(content)) => match recipient {
                Recipient::Broadcast => Self::handle_broadcast(&router, &from, content).await,
                _ => {
                    Self::handle_message(
                        &router,
                        &from,
                        &recipient,
                        &to,
                        content,
                        summary.as_deref(),
                        &sender,
                        notify_when_idle,
                        self.ctx.task_registry.as_ref(),
                    )
                    .await
                }
            },
            Some(Value::Object(obj)) => {
                if matches!(recipient, Recipient::Broadcast) {
                    Self::emit_failed(
                        &bus,
                        &invocation_id,
                        "structured_broadcast",
                        started.elapsed().as_millis() as u64,
                    )
                    .await;
                    return Err(ToolError::InvalidInput(
                        "structured messages cannot be broadcast".into(),
                    ));
                }
                if matches!(recipient, Recipient::Session(_)) {
                    return Err(ToolError::InvalidInput(
                        "structured messages cannot be sent cross-session — only plain text".into(),
                    ));
                }
                let mtype = obj.get("type").and_then(Value::as_str).unwrap_or("");
                let request_id_in = obj
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let approve = obj
                    .get("approve")
                    .and_then(Self::semantic_bool)
                    .unwrap_or(false);
                let reason = obj.get("reason").and_then(Value::as_str);
                let feedback = obj.get("feedback").and_then(Value::as_str);
                match mtype {
                    "shutdown_request" => {
                        Self::handle_shutdown_request(
                            &router, &from, &recipient, &to, reason, &sender,
                        )
                        .await
                    }
                    "shutdown_response" => {
                        if approve {
                            self.handle_shutdown_approval(
                                &router,
                                &from,
                                &request_id_in,
                                &sender,
                                &ctx,
                            )
                            .await
                        } else {
                            Self::handle_shutdown_rejection(
                                &router,
                                &from,
                                &request_id_in,
                                reason.unwrap_or(""),
                                &sender,
                            )
                            .await
                        }
                    }
                    "plan_approval_response" => {
                        if approve {
                            self.handle_plan_approval(
                                &router,
                                &from,
                                &recipient,
                                &to,
                                &request_id_in,
                                feedback,
                                &ctx,
                            )
                            .await
                        } else {
                            self.handle_plan_rejection(
                                &router,
                                &from,
                                &recipient,
                                &to,
                                &request_id_in,
                                feedback.unwrap_or("Plan needs revision"),
                                &ctx,
                            )
                            .await
                        }
                    }
                    _ => {
                        Self::emit_failed(
                            &bus,
                            &invocation_id,
                            "unknown_message_type",
                            started.elapsed().as_millis() as u64,
                        )
                        .await;
                        return Err(ToolError::InvalidInput(
                            "SendMessage: 'message' must be a string or a structured object".into(),
                        ));
                    }
                }
            }
            _ => {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "invalid_message",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    "SendMessage: 'message' must be a string or a structured object".into(),
                ));
            }
        };

        let data = match data_result {
            Ok(data) => data,
            Err(error) => {
                Self::emit_failed(
                    &bus,
                    &invocation_id,
                    "delivery_failed",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(error);
            }
        };

        Self::emit_completed(&bus, &invocation_id, started.elapsed().as_millis() as u64).await;
        // The oracle serializes the result object, excluding UI-only fields.
        let mut model_data = data.clone();
        if let Some(object) = model_data.as_object_mut() {
            object.remove("display");
            object.remove("inlineHandback");
        }
        let model_content = Some(
            serde_json::to_string(&model_data)
                .map_err(|error| ToolError::Internal(error.to_string()))?,
        );
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data,
            model_content,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::mailbox::{MailboxError, RouteAck};
    use lingxi_core::host::task_registry::{
        TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
        TaskRegistryHandle, TaskUpdatePatch,
    };
    use mobile_linux_api::ProcessOutput;
    use std::sync::{Mutex, OnceLock};
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// 2.1.238 @293558417 (`Xnm`'s `n` block): the cross-session liveness
    /// sentence. The port previously asserted `— no "busy" state`, which the
    /// oracle never said; 2.1.238 points the model at the `ListAgents` row
    /// instead. `## Cross-session` is followed by a BLANK line in the oracle
    /// template (`` `\n\n## Cross-session\n\nUse \`${Yy}\`…` ``).
    #[tokio::test]
    async fn cross_session_block_matches_the_2_1_238_bytes() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("LINGXI_HARBOR_KITE", "1");
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        let p = tool.prompt(&PromptOptions::default()).await;
        std::env::remove_var("LINGXI_HARBOR_KITE");

        assert!(
            p.contains("\n\n## Cross-session\n\nUse `ListAgents` to discover targets."),
            "heading must be followed by a blank line"
        );
        assert!(p.contains(
            "A listed peer is alive and will process your message; messages enqueue and drain at the receiver's next tool round (its `ListAgents` row says whether it is busy or idle right now). Your message arrives wrapped as `<cross-session-message from=\"...\">`."
        ));
        assert!(
            !p.contains("no \"busy\" state"),
            "the 2.1.220-era claim must be gone"
        );
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    // Delegates to the crate-level lock: these globals are per-process, so a
    // file-local mutex would only serialize this file against itself.
    fn process_lock() -> &'static Mutex<()> {
        crate::process_globals_lock()
    }

    /// Records every `(from, to, content)` it is asked to route and always acks
    /// (so name-keyed routes — which the production seam can't resolve — are
    /// observable in tests).
    struct RecordingRouter {
        routed: Mutex<Vec<(String, String, String)>>,
        broadcasted: Mutex<Vec<(String, String)>>,
        colors: Mutex<HashMap<String, String>>,
        routed_colors: Mutex<Vec<Option<String>>>,
    }

    struct RejectingRouter;

    #[derive(Default)]
    struct RecordingTaskRegistry {
        killed: Mutex<Vec<String>>,
        /// Alias-or-id → record, so `get` can answer the AGT-07 pre-flight the
        /// way the production registry does (`canonical_or_raw` resolves a
        /// teammate NAME to its task id before the map lookup).
        records: Mutex<HashMap<String, TaskRecord>>,
    }

    impl RecordingTaskRegistry {
        fn with_record(address: &str, killed_by: Option<&str>) -> Arc<Self> {
            let me = Self::default();
            me.records.lock().unwrap().insert(
                address.to_string(),
                TaskRecord {
                    task_id: "a1b2c3d4e".to_string(),
                    task_type: "in_process_teammate".to_string(),
                    status: if killed_by.is_some() {
                        "killed"
                    } else {
                        "running"
                    }
                    .to_string(),
                    description: "research".to_string(),
                    killed_by: killed_by.map(str::to_string),
                    agent_facts: None,
                    ..TaskRecord::default()
                },
            );
            Arc::new(me)
        }
    }

    #[async_trait]
    impl TaskRegistryHandle for RecordingTaskRegistry {
        async fn create(&self, _input: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used by SendMessage")
        }

        async fn get(&self, id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            Ok(self.records.lock().unwrap().get(id).cloned())
        }

        async fn list(
            &self,
            _filter: TaskListFilter,
        ) -> Result<Vec<TaskRecord>, TaskRegistryError> {
            Ok(Vec::new())
        }

        async fn update(
            &self,
            _id: &str,
            _patch: TaskUpdatePatch,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used by SendMessage")
        }

        async fn set_status(
            &self,
            _id: &str,
            _status: &str,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used by SendMessage")
        }

        async fn kill(&self, id: &str) -> Result<TaskRecord, TaskRegistryError> {
            self.killed.lock().unwrap().push(id.to_string());
            Ok(TaskRecord {
                task_id: "t12345678".into(),
                status: "killed".into(),
                agent_facts: None,
                ..Default::default()
            })
        }

        async fn output(
            &self,
            _id: &str,
            _offset: Option<u64>,
        ) -> Result<TaskOutputChunk, TaskRegistryError> {
            unreachable!("not used by SendMessage")
        }
    }

    #[async_trait]
    impl MailboxRouterHandle for RejectingRouter {
        async fn route(
            &self,
            _from_agent: &str,
            to_agent: &str,
            _message: MailboxMessage,
        ) -> Result<RouteAck, MailboxError> {
            Err(MailboxError::NotFound(to_agent.to_string()))
        }
    }

    impl RecordingRouter {
        fn new() -> Self {
            Self {
                routed: Mutex::new(Vec::new()),
                broadcasted: Mutex::new(Vec::new()),
                colors: Mutex::new(HashMap::new()),
                routed_colors: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl MailboxRouterHandle for RecordingRouter {
        async fn teammate_color(&self, name: &str) -> Option<String> {
            self.colors.lock().unwrap().get(name).cloned()
        }
        async fn route(
            &self,
            from_agent: &str,
            to_agent: &str,
            message: MailboxMessage,
        ) -> Result<RouteAck, MailboxError> {
            self.routed_colors
                .lock()
                .unwrap()
                .push(message.color.clone());
            self.routed.lock().unwrap().push((
                from_agent.to_string(),
                to_agent.to_string(),
                message.content,
            ));
            Ok(RouteAck {
                claimed_at: SystemTime::now(),
                claim_window_secs: SEND_MESSAGE_CLAIM_WINDOW.as_secs(),
            })
        }

        async fn broadcast(
            &self,
            from_agent: &str,
            message: MailboxMessage,
        ) -> Result<Vec<String>, MailboxError> {
            self.broadcasted
                .lock()
                .unwrap()
                .push((from_agent.to_string(), message.content));
            Ok(vec!["alpha".to_string(), "beta".to_string()])
        }
    }

    fn ctx_with(router: Arc<RecordingRouter>) -> BuiltinToolContext {
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.mailbox_router = Some(router as Arc<dyn MailboxRouterHandle>);
        ctx
    }

    fn ctx_with_shutdown(
        router: Arc<RecordingRouter>,
        registry: Arc<RecordingTaskRegistry>,
    ) -> BuiltinToolContext {
        let mut ctx = ctx_with(router);
        ctx.task_registry = Some(registry);
        ctx
    }

    #[test]
    fn tool_name_locked() {
        assert_eq!(SEND_MESSAGE_TOOL_NAME, "SendMessage");
    }

    #[test]
    fn claim_window_locked_30_seconds() {
        assert_eq!(SEND_MESSAGE_CLAIM_WINDOW, Duration::from_secs(30));
        assert_eq!(SEND_MESSAGE_CLAIM_WINDOW.as_secs(), 30);
    }

    #[test]
    fn schema_shape_matches_latest_shared_contract() {
        // Both sides read the SAME two env-backed flags, one after the other,
        // so a sibling test flipping `LINGXI_EXPERIMENTAL_AGENT_TEAMS` between
        // the two reads makes them disagree about the structured-message
        // variants — a real flake, not a schema divergence. Every mutator here
        // already takes this lock; this reader has to as well.
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        assert_eq!(
            tool.input_schema(),
            tool_api::send_message_contract::schema(
                lingxi_core::host::live_sessions::cross_session_messaging_enabled(),
                agent_swarms_enabled()
            )
        );
    }

    #[test]
    fn is_read_only_true_only_for_string_message() {
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        assert!(tool.is_read_only(&json!({ "to": "x", "message": "hi" })));
        assert!(
            !tool.is_read_only(&json!({ "to": "x", "message": { "type": "shutdown_request" } }))
        );
    }

    #[test]
    fn parse_recipient_resolves_name_then_agent_and_broadcast() {
        assert_eq!(
            SendMessageTool::parse_recipient("researcher").unwrap(),
            Recipient::Teammate("researcher".into())
        );
        assert_eq!(
            SendMessageTool::parse_recipient("agent:abc-123").unwrap(),
            Recipient::Teammate("agent:abc-123".into())
        );
        assert_eq!(
            SendMessageTool::parse_recipient("*").unwrap(),
            Recipient::Broadcast
        );
        let session_id = "11111111-2222-3333-4444-555555555555";
        assert_eq!(
            SendMessageTool::parse_recipient(&format!("session:{session_id}")).unwrap(),
            Recipient::Session(session_id.into())
        );
        assert!(SendMessageTool::parse_recipient("session:not-a-uuid").is_err());
    }

    // ----- validate_input parity (byte-faithful rejection strings) ----------

    #[tokio::test]
    async fn validate_rejects_empty_to() {
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .validate_input(&json!({ "to": "  ", "message": "hi" }), &fresh_ctx())
            .await
            .expect_err("empty `to` must reject");
        assert_eq!(err.0, "to must not be empty");
    }

    #[tokio::test]
    async fn validate_rejects_at_in_to() {
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .validate_input(
                &json!({ "to": "researcher@team", "summary": "s", "message": "hi" }),
                &fresh_ctx(),
            )
            .await
            .expect_err("`@` in `to` must reject");
        assert_eq!(
            err.0,
            "to must be a bare teammate name — there is only one team per session"
        );
    }

    #[tokio::test]
    async fn validate_string_message_without_summary_is_valid() {
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        tool.validate_input(&json!({"to":"researcher","message":"hi"}), &fresh_ctx())
            .await
            .expect("latest derives the optional summary");
    }

    #[tokio::test]
    async fn validate_rejects_sending_to_self() {
        let _g = process_lock().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        let dir =
            lingxi_core::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        lingxi_core::host::live_sessions::set_process_dir(dir.clone());
        lingxi_core::host::live_sessions::set_process_session_id("self-session");
        lingxi_core::host::live_sessions::set_process_name("lead");
        std::fs::create_dir_all(dir.root()).unwrap();
        std::fs::write(
            dir.root().join("111.json"),
            serde_json::to_vec(&serde_json::json!({
                "pid": 111u32,
                "sessionId": "self-session",
                "name": "lead",
                "kind": "interactive",
                "startedAt": 0
            }))
            .unwrap(),
        )
        .unwrap();

        let tool = SendMessageTool::new(ctx_with(Arc::new(RecordingRouter::new())));
        let err = tool
            .call(
                json!({ "to": "lead", "summary": "self", "message": "hi" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("self-target must reject");
        assert!(
            err.model_facing_message()
                .contains("Not sent — 'lead' is this session's own name."),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn notify_when_idle_sends_immediately_and_subscribes_once() {
        let _g = process_lock().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        let dir =
            lingxi_core::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        lingxi_core::host::live_sessions::set_process_dir(dir.clone());
        lingxi_core::host::live_sessions::set_process_session_id("self-session");
        lingxi_core::host::live_sessions::set_process_name("lead");
        std::fs::create_dir_all(dir.root()).unwrap();
        std::fs::write(
            dir.root().join("222.json"),
            serde_json::to_vec(&serde_json::json!({
                "pid": 222u32,
                "sessionId": "peer-session",
                "name": "peer",
                "kind": "interactive",
                "startedAt": 0,
                "status": "busy"
            }))
            .unwrap(),
        )
        .unwrap();

        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let result = tool
            .call(
                json!({
                    "to": "peer",
                    "notify_when_idle": true,
                    "summary": "later",
                    "message": "check this when free"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("send succeeds");

        assert_eq!(
            result.data["message"].as_str(),
            Some(
                "Subscribed — you will get one notice here when \"peer\" is next idle (or exits). Do not poll or wait for it; carry on."
            )
        );
        assert!(router.routed.lock().unwrap().is_empty());
        let delivered = dir.drain_inbox("peer-session").unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            lingxi_core::host::live_sessions::extract_cross_session_inner(&delivered[0].content),
            "check this when free"
        );
        assert_eq!(delivered[0].summary.as_deref(), Some("later"));
        let queued = dir.drain_idle_subscriptions("peer-session").unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].from, "lead");
        assert_eq!(queued[0].from_session_id, "self-session");
        assert_eq!(queued[0].summary.as_deref(), Some("later"));
    }

    /// Mirror of `coordinator::tool_send_message`'s live-socket case, for the
    /// second production sender. `session_uuid_routes_to_live_session_inbox`
    /// advertises a socket nobody answers, so it stays green with the socket
    /// leg deleted; an idle peer would then never hear the message at all.
    #[tokio::test]
    async fn a_live_socket_wakes_the_peer_and_the_inbox_still_keeps_its_copy() {
        let _g = process_lock().lock().unwrap_or_else(|e| e.into_inner());
        let peer_session_id = "11111111-2222-3333-4444-555555555555";
        let temp = tempfile::TempDir::new().unwrap();
        let dir =
            lingxi_core::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        lingxi_core::host::live_sessions::set_process_dir(dir.clone());
        lingxi_core::host::live_sessions::set_process_session_id("self-session");
        lingxi_core::host::live_sessions::set_process_name("lead");
        std::fs::create_dir_all(dir.root()).unwrap();

        // Stand in for the peer's listener, so the tool's canonical-socket gate
        // admits it and the accept loop is actually running.
        let socket = lingxi_core::host::uds_inbox::default_socket_path(std::process::id());
        lingxi_core::host::uds_inbox::stop_process_inbox();
        lingxi_core::host::uds_inbox::start_process_inbox_for_session(&socket, peer_session_id)
            .unwrap();
        std::fs::write(
            dir.root().join(format!("{}.json", std::process::id())),
            serde_json::to_vec(&serde_json::json!({
                "pid": std::process::id(),
                "sessionId": peer_session_id,
                "name": "peer",
                "kind": "interactive",
                "startedAt": 0,
                "status": "idle",
                "messagingSocketPath": socket.to_string_lossy()
            }))
            .unwrap(),
        )
        .unwrap();

        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        tool.call(
            json!({
                "to": format!("session:{peer_session_id}"),
                "summary": "wake up",
                "message": "hello over the socket"
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("session-id send succeeds");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut woken = Vec::new();
        while std::time::Instant::now() < deadline {
            woken = lingxi_core::host::uds_inbox::take_accepted_peer_reminders(false);
            if !woken.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let inbox = dir.drain_inbox(peer_session_id).unwrap();
        lingxi_core::host::uds_inbox::stop_process_inbox();

        assert_eq!(woken.len(), 1, "an idle peer must be woken: {woken:?}");
        assert!(woken[0].contains("hello over the socket"), "{woken:?}");
        assert_eq!(
            inbox.len(),
            1,
            "the durable copy must be written even when the socket answered"
        );
        assert_eq!(
            lingxi_core::host::live_sessions::extract_cross_session_inner(&inbox[0].content),
            "hello over the socket"
        );
    }

    #[tokio::test]
    async fn session_uuid_routes_to_live_session_inbox() {
        let _g = process_lock().lock().unwrap_or_else(|e| e.into_inner());
        let peer_session_id = "11111111-2222-3333-4444-555555555555";
        let temp = tempfile::TempDir::new().unwrap();
        let dir =
            lingxi_core::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        lingxi_core::host::live_sessions::set_process_dir(dir.clone());
        lingxi_core::host::live_sessions::set_process_session_id("self-session");
        lingxi_core::host::live_sessions::set_process_name("lead");
        std::fs::create_dir_all(dir.root()).unwrap();
        std::fs::write(
            dir.root().join("222.json"),
            serde_json::to_vec(&serde_json::json!({
                "pid": 222u32,
                "sessionId": peer_session_id,
                "name": "peer",
                "kind": "interactive",
                "startedAt": 0,
                "status": "idle",
                "messagingSocketPath": lingxi_core::host::uds_inbox::default_socket_path(222).to_string_lossy()
            }))
            .unwrap(),
        )
        .unwrap();

        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let result = tool
            .call(
                json!({
                    "to": format!("session:{peer_session_id}"),
                    "summary": "direct session ping",
                    "message": "hello by stable id"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("session-id send succeeds");

        assert!(result
            .model_content
            .as_deref()
            .is_some_and(|message| message.contains(peer_session_id)));
        assert!(router.routed.lock().unwrap().is_empty());
        let messages = dir.drain_inbox(peer_session_id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            lingxi_core::host::live_sessions::extract_cross_session_inner(&messages[0].content),
            "hello by stable id"
        );
        assert!(messages[0].msg_id.is_some());
    }

    #[tokio::test]
    async fn notify_when_idle_without_message_registers_subscription_only() {
        let _g = process_lock().lock().unwrap_or_else(|e| e.into_inner());
        let peer_session_id = "22222222-3333-4444-8555-666666666666";
        let temp = tempfile::TempDir::new().unwrap();
        let dir =
            lingxi_core::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        lingxi_core::host::live_sessions::set_process_dir(dir.clone());
        lingxi_core::host::live_sessions::set_process_session_id("self-session");
        lingxi_core::host::live_sessions::set_process_name("lead");
        std::fs::create_dir_all(dir.root()).unwrap();
        std::fs::write(
            dir.root().join("222.json"),
            serde_json::to_vec(&serde_json::json!({
                "pid": 222u32,
                "sessionId": peer_session_id,
                "name": "peer",
                "kind": "interactive",
                "startedAt": 0,
                "status": "busy"
            }))
            .unwrap(),
        )
        .unwrap();

        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let result = tool
            .call(
                json!({
                    "to": format!("session:{peer_session_id}"),
                    "notify_when_idle": true,
                    "summary": "ping me"
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("subscription succeeds");

        let expected = format!(
            "Subscribed — you will get one notice here when \"session:{peer_session_id}\" is next idle (or exits). Do not poll or wait for it; carry on."
        );
        assert_eq!(result.data["message"].as_str(), Some(expected.as_str()));
        assert!(router.routed.lock().unwrap().is_empty());
        let queued = dir.drain_idle_subscriptions(peer_session_id).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].summary.as_deref(), Some("ping me"));
    }

    #[tokio::test]
    async fn structured_protocol_message_rejects_session_target() {
        let tool = SendMessageTool::new(ctx_with(Arc::new(RecordingRouter::new())));
        let err = tool
            .call(
                json!({
                    "to": "session:11111111-2222-4333-8444-555555555555",
                    "message": {"type": "shutdown_request", "reason": "done"}
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("team protocol messages must not target a session");
        assert!(err
            .model_facing_message()
            .contains("structured messages cannot be sent cross-session"));
    }

    #[tokio::test]
    async fn validate_broadcast_rejected_unconditionally() {
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        // The oracle's validateInput hard-rejects `to: "*"` with a byte-exact
        // message; broadcast is no longer a supported model-facing capability.
        let err = tool
            .validate_input(
                &json!({ "to": "*", "summary": "all hands", "message": "standup in 5" }),
                &fresh_ctx(),
            )
            .await
            .expect_err("broadcast (to: \"*\") must be rejected");
        assert_eq!(
            err.0,
            "broadcast (to: \"*\") is no longer supported — send a message per recipient"
        );
    }

    #[tokio::test]
    async fn validate_shutdown_response_must_target_team_lead() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("LINGXI_EXPERIMENTAL_AGENT_TEAMS", "1");
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .validate_input(
                &json!({
                    "to": "researcher",
                    "message": { "type": "shutdown_response", "request_id": "r", "approve": true }
                }),
                &fresh_ctx(),
            )
            .await
            .expect_err("shutdown_response to a non-lead must reject");
        assert_eq!(err.0, "shutdown_response must be sent to \"team-lead\"");
        std::env::remove_var("LINGXI_EXPERIMENTAL_AGENT_TEAMS");
    }

    #[tokio::test]
    async fn validate_shutdown_rejection_requires_reason() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("LINGXI_EXPERIMENTAL_AGENT_TEAMS", "1");
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .validate_input(
                &json!({
                    "to": "team-lead",
                    "message": { "type": "shutdown_response", "request_id": "r", "approve": false }
                }),
                &fresh_ctx(),
            )
            .await
            .expect_err("rejecting shutdown without a reason must reject");
        assert_eq!(
            err.0,
            "reason is required when rejecting a shutdown request"
        );
        std::env::remove_var("LINGXI_EXPERIMENTAL_AGENT_TEAMS");
    }

    #[tokio::test]
    async fn validate_accepts_well_formed_string_message() {
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        tool.validate_input(
            &json!({ "to": "researcher", "summary": "assign task 1", "message": "go" }),
            &fresh_ctx(),
        )
        .await
        .expect("a summarised string message is valid");
    }

    // ----- call() routing + handler shapes ----------------------------------

    #[tokio::test]
    async fn main_recipient_rejects_main_caller_without_a_delivery() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let result = tool
            .call(
                json!({"to":"main","message":"hello"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.model_content.as_deref(),
            Some(
                r#"{"success":false,"message":"You are the main conversation — \"main\" addresses you. Send to a named agent instead."}"#
            )
        );
        assert!(router.routed.lock().unwrap().is_empty());
        assert!(result.data.get("routing").is_none());
        assert!(result.data.get("msg_id").is_none());
    }

    #[tokio::test]
    async fn ordinary_child_can_send_to_main_with_exact_queued_result() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let mut ctx = fresh_ctx();
        ctx.agent_id = Some(lingxi_core::types::AgentId::new());
        ctx.agent_name = Some("scout".into());
        let result = tool
            .call(
                json!({"to":"main","message":"Finished checking"}),
                ctx,
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.model_content.as_deref(),
            Some(
                r#"{"success":true,"message":"Message queued for the main conversation's next turn."}"#
            )
        );
        assert_eq!(
            router.routed.lock().unwrap().as_slice(),
            &[("scout".into(), "main".into(), "Finished checking".into())]
        );
        assert!(result.data.get("routing").is_none());
        assert!(result.data.get("msg_id").is_none());
    }

    #[tokio::test]
    async fn main_route_keeps_plain_message_validation() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let error = tool
            .call(json!({"to":"main","message":"  "}), fresh_ctx(), fresh_tx())
            .await
            .unwrap_err();
        assert!(error
            .model_facing_message()
            .contains("message must not be empty"));
        assert!(router.routed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn teammate_colors_are_read_from_the_actual_router() {
        let router = Arc::new(RecordingRouter::new());
        router.colors.lock().unwrap().extend([
            ("scout".into(), "blue".into()),
            ("reviewer".into(), "green".into()),
            ("team-lead".into(), "red".into()),
        ]);
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let mut ctx = fresh_ctx();
        ctx.agent_id = Some(lingxi_core::types::AgentId::new());
        ctx.agent_name = Some("scout".into());
        let result = tool
            .call(
                json!({"to":"reviewer","message":"Finished"}),
                ctx,
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.data["routing"].to_string(),
            r#"{"sender":"scout","senderColor":"blue","target":"@reviewer","targetColor":"green","summary":"Finished","content":"Finished"}"#
        );
        assert_eq!(
            router.routed_colors.lock().unwrap()[0].as_deref(),
            Some("blue")
        );
        let result = tool
            .call(
                json!({"to":"reviewer","message":"Finished"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert!(
            result.data["routing"].get("senderColor").is_none(),
            "leader roster color is not a teammate context color"
        );
        assert_eq!(result.data["routing"]["targetColor"], "green");
    }

    #[tokio::test]
    async fn raw_agent_id_emitted_by_agent_tool_routes_without_prefix_translation() {
        let id = lingxi_core::types::AgentId::new();
        let emitted = id.as_uuid().to_string();
        assert_eq!(
            SendMessageTool::parse_recipient(&emitted).unwrap(),
            Recipient::Agent(emitted.clone())
        );
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let result = tool
            .call(
                json!({"to":emitted,"message":"Continue"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["success"], true);
        assert_eq!(router.routed.lock().unwrap()[0].1, id.as_uuid().to_string());
    }

    #[tokio::test]
    async fn delivers_string_message_resolved_by_name() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let res = tool
            .call(
                json!({ "to": "researcher", "summary": "assign task 1", "message": "start on task #1" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("send must succeed");
        assert_eq!(res.data["success"], true);
        assert_eq!(res.data["message"], "Message sent to researcher's inbox");
        assert_eq!(res.data["routing"]["target"], "@researcher");
        assert_eq!(res.data["routing"]["content"], "start on task #1");
        // The UI status remains available alongside the model JSON result.
        assert_eq!(
            res.data["message"].as_str(),
            Some("Message sent to researcher's inbox")
        );

        let routed = router.routed.lock().unwrap();
        assert_eq!(routed.len(), 1);
        // Resolved by NAME — the recipient string is the bare teammate name.
        assert_eq!(routed[0].1, "researcher");
        assert_eq!(routed[0].2, "start on task #1");
    }

    #[tokio::test]
    async fn uses_spawned_agents_display_name_for_sender_identity() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let mut call_ctx = fresh_ctx();
        call_ctx.agent_id = Some(lingxi_core::types::AgentId::new());
        call_ctx.agent_name = Some("researcher".to_string());

        let res = tool
            .call(
                json!({ "to": "reviewer", "summary": "share findings", "message": "done" }),
                call_ctx,
                fresh_tx(),
            )
            .await
            .expect("send must succeed");

        assert_eq!(res.data["routing"]["sender"], "researcher");
        assert_eq!(router.routed.lock().unwrap()[0].0, "researcher");
    }

    #[tokio::test]
    async fn delivery_failure_is_not_reported_as_success() {
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.mailbox_router = Some(Arc::new(RejectingRouter));
        let tool = SendMessageTool::new(ctx);

        let err = tool
            .call(
                json!({ "to": "missing", "summary": "send update", "message": "hello" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("unknown recipient must fail");

        assert!(format!("{err}").contains("failed to deliver to 'missing'"));
    }

    #[tokio::test]
    async fn routing_content_is_truncated_to_50_char_preview() {
        // 2.1.212: the sender's tool result carries only a 50-char preview of
        // the body (`Us(t, 50)`); the full body rides the mailbox delivery and
        // is not duplicated in the transcript.
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let body = "a".repeat(100);
        let res = tool
            .call(
                json!({ "to": "researcher", "summary": "long note", "message": body.clone() }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("send must succeed");

        // routing.content = first 49 chars + ellipsis (width-50 truncation).
        let expected_preview = format!("{}\u{2026}", "a".repeat(49));
        assert_eq!(res.data["routing"]["content"], expected_preview);
        assert_eq!(
            res.data["routing"]["content"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            50
        );

        // The mailbox still receives the FULL body — nothing is truncated on the
        // delivery path.
        let routed = router.routed.lock().unwrap();
        assert_eq!(routed.len(), 1);
        assert_eq!(routed[0].2, body);
    }

    #[test]
    fn truncate_preview_leaves_short_bodies_intact() {
        // At or under the width the body is returned verbatim (no ellipsis).
        assert_eq!(truncate_preview("start on task #1", 50), "start on task #1");
        assert_eq!(truncate_preview(&"a".repeat(50), 50), "a".repeat(50));
        assert_eq!(
            truncate_preview(&"a".repeat(51), 50),
            format!("{}\u{2026}", "a".repeat(49))
        );
    }

    #[test]
    fn truncate_preview_counts_terminal_cells_and_keeps_graphemes_intact() {
        assert_eq!(truncate_preview("你好世界", 7), "你好世…");
        assert_eq!(truncate_preview("👨‍👩‍👧‍👦abc", 4), "👨‍👩‍👧‍👦a…");
    }

    #[tokio::test]
    async fn call_rejects_broadcast_without_delivery() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let error = tool
            .call(json!({"to":"*","message":"hi"}), fresh_ctx(), fresh_tx())
            .await
            .unwrap_err();
        assert!(error.model_facing_message().contains("broadcast (to:"));
        assert!(router.broadcasted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn result_content_serializes_the_result_object() {
        let tool = SendMessageTool::new(ctx_with(Arc::new(RecordingRouter::new())));
        let result = tool
            .call(
                json!({"to":"worker","message":"First line\nsecond"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(result.model_content.as_deref().unwrap()).unwrap(),
            result.data
        );
        assert_eq!(result.data["routing"]["summary"], "First line");
    }

    #[tokio::test]
    async fn structured_shutdown_request_shape() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let res = tool
            .call(
                json!({
                    "to": "researcher",
                    "message": { "type": "shutdown_request", "reason": "done for the day" }
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("shutdown_request must succeed");
        assert_eq!(res.data["success"], true);
        assert_eq!(res.data["target"], "researcher");
        assert!(res.data["request_id"]
            .as_str()
            .unwrap()
            .starts_with("shutdown-"));
        let msg = res.data["message"].as_str().unwrap();
        assert!(msg.starts_with("Shutdown request sent to researcher. Request ID: shutdown-"));
        // The status is preserved in the serialized result object.
        assert_eq!(res.data["message"].as_str(), Some(msg));
        // The structured payload was delivered to the named recipient.
        let routed = router.routed.lock().unwrap();
        assert_eq!(routed[0].1, "researcher");
        let frame: Value = serde_json::from_str(&routed[0].2).unwrap();
        assert_eq!(frame["type"], "shutdown_request");
        assert_eq!(frame["from"], "team-lead");
        assert_eq!(frame["reason"], "done for the day");
        assert!(frame["timestamp"].as_str().is_some_and(|timestamp| {
            timestamp.len() == 24 && timestamp.as_bytes()[19] == b'.' && timestamp.ends_with('Z')
        }));
    }

    #[tokio::test]
    async fn structured_shutdown_rejection_shape() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router));
        let res = tool
            .call(
                json!({
                    "to": "team-lead",
                    "message": {
                        "type": "shutdown_response",
                        "request_id": "shutdown-1@researcher",
                        "approve": false,
                        "reason": "still mid-task"
                    }
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("shutdown rejection must succeed");
        assert_eq!(res.data["success"], true);
        assert_eq!(res.data["request_id"], "shutdown-1@researcher");
        assert_eq!(
            res.data["message"],
            "Shutdown rejected. Reason: \"still mid-task\". Continuing to work."
        );
    }

    #[tokio::test]
    async fn plan_approval_is_lead_only_and_inherits_non_plan_mode() {
        let router = Arc::new(RecordingRouter::new());
        let mut tool_ctx = ctx_with(router.clone());
        tool_ctx.permission_mode = permission::PermissionMode::AcceptEdits;
        let tool = SendMessageTool::new(tool_ctx);

        tool.call(
            json!({
                "to": "researcher",
                "message": {
                    "type": "plan_approval_response",
                    "request_id": "plan-1@researcher",
                    "approve": true
                }
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("team lead may approve");

        let routed = router.routed.lock().unwrap();
        let frame: Value = serde_json::from_str(&routed[0].2).unwrap();
        assert_eq!(frame["type"], "plan_approval_response");
        assert_eq!(frame["permissionMode"], "acceptEdits");
        assert!(frame.get("from").is_none());
        assert!(frame["timestamp"].as_str().is_some_and(|timestamp| {
            timestamp.len() == 24 && timestamp.as_bytes()[19] == b'.' && timestamp.ends_with('Z')
        }));
    }

    #[tokio::test]
    async fn plan_approval_downgrades_leader_plan_mode_to_default() {
        let router = Arc::new(RecordingRouter::new());
        let mut tool_ctx = ctx_with(router.clone());
        tool_ctx.permission_mode = permission::PermissionMode::Plan;
        let tool = SendMessageTool::new(tool_ctx);

        tool.call(
            json!({
                "to": "researcher",
                "message": {
                    "type": "plan_approval_response",
                    "request_id": "plan-2@researcher",
                    "approve": true
                }
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("team lead may approve");

        let frame: Value = serde_json::from_str(&router.routed.lock().unwrap()[0].2).unwrap();
        assert_eq!(frame["permissionMode"], "default");
    }

    #[tokio::test]
    async fn plan_approval_preserves_optional_feedback_without_sender_field() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));

        tool.call(
            json!({
                "to": "researcher",
                "message": {
                    "type": "plan_approval_response",
                    "request_id": "plan-feedback@researcher",
                    "approve": true,
                    "feedback": "Proceed with the smaller diff"
                }
            }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("team lead may approve with feedback");

        let frame: Value = serde_json::from_str(&router.routed.lock().unwrap()[0].2).unwrap();
        assert_eq!(frame["feedback"], "Proceed with the smaller diff");
        assert!(frame.get("from").is_none());
    }

    #[tokio::test]
    async fn plan_rejection_truncates_only_the_model_facing_feedback_preview() {
        let router = Arc::new(RecordingRouter::new());
        let tool = SendMessageTool::new(ctx_with(router.clone()));
        let feedback = "x".repeat(80);

        let result = tool
            .call(
                json!({
                    "to": "researcher",
                    "message": {
                        "type": "plan_approval_response",
                        "request_id": "plan-reject@researcher",
                        "approve": false,
                        "feedback": feedback
                    }
                }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("team lead may reject");

        let frame: Value = serde_json::from_str(&router.routed.lock().unwrap()[0].2).unwrap();
        assert_eq!(frame["feedback"], "x".repeat(80));
        assert!(frame.get("from").is_none());
        assert_eq!(
            result.data["message"],
            format!(
                "Plan rejected for researcher with feedback: \"{}\"",
                truncate_preview(&"x".repeat(80), ROUTING_CONTENT_PREVIEW_CHARS)
            )
        );
    }

    #[tokio::test]
    async fn teammate_cannot_approve_or_reject_plans() {
        for (approve, action) in [(true, "approve"), (false, "reject")] {
            let router = Arc::new(RecordingRouter::new());
            let tool = SendMessageTool::new(ctx_with(router.clone()));
            let mut call_ctx = fresh_ctx();
            call_ctx.agent_id = Some(lingxi_core::types::AgentId::new());
            call_ctx.agent_name = Some("reviewer".to_string());
            let error = tool
                .call(
                    json!({
                        "to": "researcher",
                        "message": {
                            "type": "plan_approval_response",
                            "request_id": "plan-3@researcher",
                            "approve": approve
                        }
                    }),
                    call_ctx,
                    fresh_tx(),
                )
                .await
                .expect_err("teammate plan decisions must fail closed");
            assert!(error
                .model_facing_message()
                .contains(&format!("Only the team lead can {action} plans")));
            assert!(router.routed.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn shutdown_approval_delivers_then_kills_the_calling_worker() {
        let router = Arc::new(RecordingRouter::new());
        let registry = Arc::new(RecordingTaskRegistry::default());
        let tool = SendMessageTool::new(ctx_with_shutdown(router.clone(), registry.clone()));
        let mut call_ctx = fresh_ctx();
        let agent_id = lingxi_core::types::AgentId::new();
        call_ctx.agent_id = Some(agent_id);
        call_ctx.agent_name = Some("researcher".into());

        let result = tool
            .call(
                json!({
                    "to": "team-lead",
                    "message": {
                        "type": "shutdown_response",
                        "request_id": "shutdown-1@researcher",
                        "approve": true
                    }
                }),
                call_ctx,
                fresh_tx(),
            )
            .await
            .expect("approval is delivered and worker is stopped");

        assert_eq!(result.data["success"], true);
        assert_eq!(router.routed.lock().unwrap()[0].1, "team-lead");
        assert_eq!(
            registry.killed.lock().unwrap().as_slice(),
            &[agent_id.to_string()]
        );
    }

    /// AGT-07. A user stop is a decision, not a transient failure: the model
    /// must learn about it in the RESULT of the call it made, not one hop later
    /// inside the pump (which logs `Terminated` at debug and stops).
    #[tokio::test]
    async fn a_message_to_a_user_stopped_teammate_is_refused_synchronously() {
        let router = Arc::new(RecordingRouter::new());
        let registry = RecordingTaskRegistry::with_record("researcher", Some("user"));
        let tool = SendMessageTool::new(ctx_with_shutdown(router.clone(), registry));

        let err = tool
            .call(
                json!({ "to": "researcher", "summary": "s", "message": "keep going" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("a user-stopped agent must not silently accept messages");

        assert_eq!(
            err.model_facing_message(),
            lingxi_core::host::task_registry::stopped_by_user_message("a1b2c3d4e")
        );
        // The refusal is a PRE-flight: nothing may reach the mailbox, because
        // `deliver` returns as soon as the mailbox accepts and the registry's
        // own refusal could then never surface in this tool's return value.
        assert!(
            router.routed.lock().unwrap().is_empty(),
            "the message must not be routed"
        );
    }

    /// The gate is keyed on WHO stopped it. A parent/model stop carries no user
    /// intent, so it keeps the ordinary delivery path (the seam still answers
    /// `Terminated` if the agent is really gone).
    #[tokio::test]
    async fn a_parent_stopped_teammate_still_takes_the_ordinary_path() {
        let router = Arc::new(RecordingRouter::new());
        let registry = RecordingTaskRegistry::with_record("researcher", Some("parent"));
        let tool = SendMessageTool::new(ctx_with_shutdown(router.clone(), registry));

        let res = tool
            .call(
                json!({ "to": "researcher", "summary": "s", "message": "keep going" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("a parent stop must not be reported as a user cancellation");

        assert_eq!(res.data["success"], true);
        assert_eq!(router.routed.lock().unwrap().len(), 1);
    }

    /// Best-effort by construction: an address the registry cannot resolve
    /// falls through. The gate exists to convert a KNOWN cancellation into
    /// something the model can act on, not to add a new way to fail.
    #[tokio::test]
    async fn an_unresolvable_target_falls_through_to_delivery() {
        let router = Arc::new(RecordingRouter::new());
        let registry = Arc::new(RecordingTaskRegistry::default());
        let tool = SendMessageTool::new(ctx_with_shutdown(router.clone(), registry));

        let res = tool
            .call(
                json!({ "to": "researcher", "summary": "s", "message": "keep going" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("an unknown address must not be refused");

        assert_eq!(res.data["success"], true);
        assert_eq!(router.routed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn router_not_wired_is_internal_error() {
        // Default shell ctx leaves `mailbox_router` = None.
        let tool = SendMessageTool::new(shell_test_ctx(dummy_out()));
        let err = tool
            .call(
                json!({ "to": "researcher", "summary": "s", "message": "hi" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("an unwired router must error");
        match err {
            ToolError::Internal(m) => assert!(m.contains("not wired"), "got {m}"),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    /// M-02: a session with NO mailbox router must not advertise `SendMessage`.
    /// Without a router every call returns `router_not_wired`, so offering it
    /// costs a tool slot, prompt budget and one wasted call to find that out.
    #[tokio::test]
    async fn send_message_is_disabled_without_a_mailbox_router() {
        let ctx = shell_test_ctx(dummy_out());
        assert!(
            ctx.mailbox_router.is_none(),
            "fixture precondition: no router wired"
        );
        let tool = SendMessageTool::new(ctx);
        assert!(!tool.is_enabled(&ToolStaticContext::default()));
    }

    #[test]
    fn ordinary_agent_messages_are_enabled_when_agent_teams_gate_is_off() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var("USER_TYPE");
        std::env::remove_var("LINGXI_EXPERIMENTAL_AGENT_TEAMS");

        let tool = SendMessageTool::new(ctx_with(Arc::new(RecordingRouter::new())));
        assert!(
            tool.is_enabled(&ToolStaticContext::default()),
            "ordinary background messages do not require the teams gate"
        );
    }

    #[test]
    fn send_message_is_enabled_when_agent_teams_gate_is_on_and_router_wired() {
        let _guard = env_lock().lock().unwrap();
        std::env::remove_var("USER_TYPE");
        std::env::set_var("LINGXI_EXPERIMENTAL_AGENT_TEAMS", "1");

        let tool = SendMessageTool::new(ctx_with(Arc::new(RecordingRouter::new())));
        assert!(tool.is_enabled(&ToolStaticContext::default()));

        std::env::remove_var("LINGXI_EXPERIMENTAL_AGENT_TEAMS");
    }
}
