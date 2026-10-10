//! stdin NDJSON reader for `--input-format stream-json`.
//!
//! Reads the process stdin line-by-line, parses each non-empty line as a JSON
//! frame, normalises camelCase keys (`requestId`→`request_id`), and dispatches
//! by `type`:
//!
//! - `user`   → the primary turn; role-checked + uuid-deduped + fed into the
//!   orchestrator's sequential input loop.
//! - `assistant` / `system` → ordered history seed entries.
//! - `bash_command` → a sandboxed shell command executed between turns.
//! - `keep_alive` → silently ignored.
//! - `update_environment_variables` → routes the validated credential map to
//!   the host service (SDK auth/config refresh parity).
//! - `control_request` → `request` field required; routed onto the control channel.
//!   `control_cancel_request` is also routed to this control channel for active
//!   permission round-trip cancellation.
//! - `control_response` → routed onto the pending-resolver channel.
//! - unknown  → warn to stderr, drop.
//!
//! ## Dedup + replay (`--replay-user-messages`)
//!
//! Each `user` frame carries an optional `uuid` field. If the uuid is already
//! in the seen-set the turn is SKIPPED. When `--replay-user-messages` is true
//! the duplicate-ack frame (same uuid, `isReplay:true`) is emitted on stdout.
//!
//! ## Error handling
//!
//! A malformed JSON line is FATAL: print
//! a sanitized parse diagnostic to stderr then return
//! `Err(InputError::MalformedJson)` (caller exits 1). Role mismatch and a
//! missing `control_request.request` field are also fatal.
//!
//! ## Remaining private/host-dependent gaps
//! - `get_context_usage`/`get_session_cost`/`set_permission_mode` wire shapes
//!   are conservative approximations, intentionally not byte-perfect where private
//!   behavior is uncertain.

#![forbid(unsafe_code)]

use crate::headless::io::{Input, Output};
use crate::headless::stream_json::{serialize_ndjson_line, OutboundMsg, OutboundTx};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{
    CompactBoundaryMetadata, ContentBlock, ConversationMessage, MessageId, ToolUseId,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::io;
#[cfg(test)]
use std::io::BufRead;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

// ── Error type ────────────────────────────────────────────────────────────────

/// Fatal input-processing errors. The caller should print nothing extra —
/// `process_line` already emitted the required error string to stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputError {
    /// JSON parse failure (`Error parsing streaming input line: ...`).
    MalformedJson,
    /// `user` frame had a non-`"user"` role (`Error: Expected message role ...`).
    BadRole(String),
    /// `control_request` frame missing the `request` field.
    MissingRequest,
    /// Stdin could not be read or its dedicated reader could not be started.
    ReadFailed,
    /// The outbound single writer has closed before frame delivery.
    DeliveryFailed,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::MalformedJson => write!(f, "Error parsing streaming input line"),
            InputError::BadRole(_) => write!(f, "Error: Expected message role 'user'"),
            InputError::MissingRequest => {
                write!(f, "Error: Missing request on control_request")
            }
            InputError::ReadFailed => write!(f, "Error reading streaming input"),
            InputError::DeliveryFailed => write!(f, "Error delivering headless output"),
        }
    }
}

// ── normalizeControlMessageKeys ───────────────────────────────────────────────

/// Apply the camelCase→snake_case normalisation that the TS `normalizeControlMessageKeys`
/// does: renames the top-level `requestId` key to `request_id`, and the nested
/// `response.requestId` to `response.request_id`. (iOS clients emit camelCase;
/// the canonical wire format is snake_case.)
fn normalize_control_message_keys(v: &mut Value) {
    if let Some(obj) = v.as_object_mut() {
        // Top-level `requestId` → `request_id`
        if let Some(val) = obj.remove("requestId") {
            obj.entry("request_id").or_insert(val);
        }
        // Nested `response.requestId` → `response.request_id`
        if let Some(resp) = obj.get_mut("response") {
            if let Some(resp_obj) = resp.as_object_mut() {
                if let Some(val) = resp_obj.remove("requestId") {
                    resp_obj.entry("request_id").or_insert(val);
                }
            }
        }
    }
}

fn normalize_projected_control_message_keys(projection: &mut Utf16JsonProjection) {
    let mut moves = Vec::new();
    for (old, new) in [
        ("/requestId", "/request_id"),
        ("/response/requestId", "/response/request_id"),
    ] {
        if projection.value.pointer(old).is_some() {
            moves.push((old, new, projection.value.pointer(new).is_some()));
        }
    }
    normalize_control_message_keys(&mut projection.value);
    for (old, new, target_exists) in moves {
        projection.strings.retain_mut(|entry| {
            if entry.pointer == old || entry.pointer.starts_with(&format!("{old}/")) {
                if target_exists {
                    return false;
                }
                entry.pointer = format!("{new}{}", &entry.pointer[old.len()..]);
            }
            true
        });
        projection.keys.retain_mut(|entry| {
            if entry.pointer == old || entry.pointer.starts_with(&format!("{old}/")) {
                if target_exists {
                    return false;
                }
                entry.pointer = format!("{new}{}", &entry.pointer[old.len()..]);
            }
            true
        });
    }
}

// ── Parsed turn ───────────────────────────────────────────────────────────────

/// The content extracted from a `user` input frame.
#[derive(Debug, Clone)]
pub struct UserTurn {
    /// The message content: either a single string or a JSON array of content blocks.
    pub content: Value,
    pub content_projection: Utf16JsonProjection,
    pub frame_projection: Utf16JsonProjection,
    /// The optional uuid from the frame (used for dedup).
    pub uuid: Option<String>,
    pub queue_delivery: Option<orchestrator::prompt::mid_turn_input::MidTurnInputDelivery>,
}

/// An externally supplied history entry, kept in input order with user turns.
#[derive(Debug, Clone)]
pub struct HistoryInput {
    /// Canonical engine message appended to session history and JSONL.
    pub message: ConversationMessage,
    /// Original normalized assistant frame for `--replay-user-messages`.
    pub replay_frame: Option<Value>,
    pub frame_projection: Utf16JsonProjection,
}

/// Legacy SDK `bash_command` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashCommand {
    /// Command text executed through the session's sandboxed Bash runner.
    pub command: String,
}

/// Ordered data-plane input. A single channel is required so history seeds and
/// bash output cannot overtake neighboring user turns.
#[derive(Debug, Clone)]
pub enum StreamInput {
    /// Model turn.
    User(UserTurn),
    /// Inbound transcript seed.
    History(HistoryInput),
    /// Sandboxed shell command.
    Bash(BashCommand),
}

// ── Frame dispatch ────────────────────────────────────────────────────────────

/// Dispatch actions produced by [`process_line`].
#[derive(Debug)]
pub enum FrameAction {
    /// A validated `user` turn to feed into the orchestrator.
    UserTurn(UserTurn),
    /// A parsed assistant/system history entry.
    History(HistoryInput),
    /// A legacy SDK bash command.
    BashCommand(BashCommand),
    /// A duplicate `user` frame (same uuid). Carries the ORIGINAL uuid,
    /// content, and timestamp so the replay-ack can echo them verbatim —
    /// claude-code's `SDKUserMessageReplaySchema` requires the original uuid +
    /// content (re-minting them defeats the host's replay correlation).
    DuplicateUser {
        uuid: String,
        content: Value,
        timestamp: Option<String>,
        frame_projection: Utf16JsonProjection,
    },
    /// A `control_request` frame — routed to the control dispatcher.
    /// Carries the full parsed (normalised) frame value including `request_id`
    /// and `request` sub-object. The `request` field is guaranteed present
    /// (missing-request is validated and fatal before this variant is returned).
    ControlRequest(Utf16JsonProjection),
    /// A `control_response` frame — routed to the pending-request resolver.
    /// Carries the full parsed (normalised) frame.
    ControlResponse(Utf16JsonProjection),
    /// A `control_cancel_request` frame for an in-flight outbound control request.
    /// Carries the `request_id` to cancel.
    ControlCancel(Utf16JsonProjection),
    UpdateEnvironmentVariables(HashMap<String, String>),
    /// The frame was silently consumed (keep_alive, update_environment_variables,
    /// assistant/system, unknown with warning).
    Consumed,
}

/// Parse and dispatch one NDJSON line.
///
/// Returns:
/// - `Ok(FrameAction)` on success.
/// - `Err(InputError)` for fatal errors (caller exits 1 after printing the
///   required error string — we print it here).
///
/// `seen_uuids` is the per-session dedup set (updated on new `user` turns).
/// `session_id` and `replay_user_messages` are needed by the replay-ack emitter.
pub fn process_line(
    line: &str,
    seen_uuids: &mut HashSet<String>,
    diagnostics: &mut dyn FnMut(&str),
) -> Result<FrameAction, InputError> {
    if line.trim().is_empty() {
        return Ok(FrameAction::Consumed);
    }

    let mut projection = Utf16JsonProjection::parse(line).map_err(|_| {
        let detail = serde_json::from_str::<Value>(line)
            .err()
            .map(|e| (e.line(), e.column()))
            .unwrap_or((1, 1));
        diagnostics(&format!(
            "Error parsing streaming input line: invalid JSON at line {} column {}",
            detail.0, detail.1
        ));
        InputError::MalformedJson
    })?;
    normalize_projected_control_message_keys(&mut projection);
    let frame = &projection.value;

    let frame_type = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    match frame_type.as_str() {
        "keep_alive" => Ok(FrameAction::Consumed),

        "update_environment_variables" => {
            // SECURITY: Claude Code's structuredIO does NOT apply arbitrary env
            // keys — it enforces a two-key ALLOWLIST and refuses everything else.
            // An SDK peer driving stream-json input must not be able to set e.g.
            // `BASH_ENV=/tmp/evil.sh` (sourced by the next Bash-tool `/bin/sh -c`
            // child) or overwrite auth/config the CLI protects.
            //
            // Oracle: `UtS = new Set(["CLAUDE_CODE_SESSION_ACCESS_TOKEN",
            // "CLAUDE_CODE_OAUTH_TOKEN"])`; the frame is DROPPED (`must be an
            // object of string values`) if `variables` is not an object of
            // strings; allowlisted keys are applied, non-allowlisted keys are
            // collected and refused with a log.
            const ALLOWLIST: [&str; 2] = [
                "CLAUDE_CODE_SESSION_ACCESS_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
            ];
            let Some(env_vars) = frame.get("variables").and_then(Value::as_object) else {
                diagnostics(&format!(
                    "[structuredIO] dropped update_environment_variables: variables must be an object of string values"
                ));
                return Ok(FrameAction::Consumed);
            };
            // Every value must be a string (oracle schema `z.record(z.string())`);
            // any non-string drops the WHOLE frame — nothing is applied.
            // (The oracle also emits a `control_response` error when the frame
            // carries a `request_id`; this parse fn only routes frames, so the
            // drop is surfaced via the same stderr log the port uses for other
            // structuredIO validation failures.)
            if env_vars.values().any(|v| !v.is_string()) {
                diagnostics(&format!(
                    "[structuredIO] dropped update_environment_variables: variables must be an object of string values"
                ));
                return Ok(FrameAction::Consumed);
            }
            let mut refused: Vec<&str> = Vec::new();
            let mut updates = HashMap::new();
            for (k, v) in env_vars {
                let Some(val) = v.as_str() else { continue };
                if ALLOWLIST.contains(&k.as_str()) {
                    updates.insert(k.clone(), val.to_owned());
                } else {
                    refused.push(k.as_str());
                }
            }
            if !refused.is_empty() {
                diagnostics(&format!(
                    "[structuredIO] refused update_environment_variables for non-allowlisted keys: {}",
                    refused.join(", ")
                ));
            }
            Ok(FrameAction::UpdateEnvironmentVariables(updates))
        }

        "control_request" => {
            // require `request` field (byte-exact error matches binary).
            if frame.get("request").is_none() {
                diagnostics(&format!("Error: Missing request on control_request"));
                return Err(InputError::MissingRequest);
            }
            // Route to the control dispatcher.
            Ok(FrameAction::ControlRequest(projection))
        }

        "control_response" => {
            // Route to the pending-request resolver.
            Ok(FrameAction::ControlResponse(projection))
        }

        "control_cancel_request" => {
            let request_id = projection
                .subprojection("/request_id")
                .unwrap_or_else(|_| Utf16JsonProjection::plain(json!("")));
            Ok(FrameAction::ControlCancel(request_id))
        }

        "user" => {
            // Role check.
            let role = frame
                .get("message")
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if role != "user" {
                let got = role.to_string();
                diagnostics(&format!("Error: Expected message role 'user'"));
                return Err(InputError::BadRole(got));
            }

            // Extract content.
            let content = frame
                .get("message")
                .and_then(|m| m.get("content"))
                .cloned()
                .unwrap_or(Value::String(String::new()));

            // Original timestamp (echoed verbatim on a replay-ack when present).
            let timestamp = frame
                .get("timestamp")
                .and_then(Value::as_str)
                .map(String::from);

            // UUID dedup.
            let uuid = frame.get("uuid").and_then(Value::as_str).map(String::from);
            if let Some(ref u) = uuid {
                let identity = projection
                    .subprojection("/uuid")
                    .and_then(|id| id.to_json_string())
                    .map_err(|_| InputError::MalformedJson)?;
                if seen_uuids.contains(&identity) {
                    // Echo the ORIGINAL uuid + content + timestamp (not re-minted).
                    return Ok(FrameAction::DuplicateUser {
                        uuid: u.clone(),
                        content,
                        timestamp,
                        frame_projection: projection,
                    });
                }
                seen_uuids.insert(identity);
            }

            Ok(FrameAction::UserTurn(UserTurn {
                queue_delivery: None,
                content_projection: projection
                    .subprojection("/message/content")
                    .unwrap_or_else(|_| Utf16JsonProjection::plain(content.clone())),
                content,
                uuid,
                frame_projection: projection,
            }))
        }

        "assistant" | "system" => Ok(parse_history_frame(&projection)
            .map(FrameAction::History)
            .unwrap_or(FrameAction::Consumed)),

        "bash_command" => {
            let command = frame
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(FrameAction::BashCommand(BashCommand { command }))
        }

        other => {
            diagnostics(&format!("Ignoring unknown message type: {other}"));
            Ok(FrameAction::Consumed)
        }
    }
}

fn parse_history_frame(projection: &Utf16JsonProjection) -> Option<HistoryInput> {
    let frame = &projection.value;
    let frame_type = frame.get("type")?.as_str()?;
    let message_id = frame
        .get("uuid")
        .and_then(Value::as_str)
        .and_then(MessageId::parse_prefixed)
        .unwrap_or_default();

    match frame_type {
        "assistant" => {
            let message = frame.get("message")?;
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return None;
            }
            let content_projection = projection.subprojection("/message/content").ok();
            let content = parse_assistant_content(content_projection.as_ref());
            Some(HistoryInput {
                message: ConversationMessage::Assistant {
                    per_turn_effort: None,
                    id: message_id,
                    content,
                    stop_reason: message
                        .get("stop_reason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
                replay_frame: Some(frame.clone()),
                frame_projection: projection.clone(),
            })
        }
        "system" => {
            // `toInternalMessages` accepts only the SDK compact boundary;
            // informational/status system frames are presentation events and
            // must not become model conversation history.
            if frame.get("subtype").and_then(Value::as_str) != Some("compact_boundary") {
                return None;
            }
            let mut compact_metadata: CompactBoundaryMetadata = serde_json::from_value(
                normalize_compact_metadata_keys(frame.get("compact_metadata")?.clone()),
            )
            .ok()?;
            compact_metadata.logical_parent_uuid = frame
                .get("logical_parent_uuid")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(HistoryInput {
                message: ConversationMessage::compact_boundary(
                    message_id,
                    "Conversation compacted".to_string(),
                    compact_metadata,
                ),
                replay_frame: None,
                frame_projection: projection.clone(),
            })
        }
        _ => None,
    }
}

fn normalize_compact_metadata_keys(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    let mut parts = key.split('_');
                    let mut camel = parts.next().unwrap_or_default().to_string();
                    for part in parts {
                        let mut chars = part.chars();
                        if let Some(first) = chars.next() {
                            camel.extend(first.to_uppercase());
                            camel.extend(chars);
                        }
                    }
                    let value = if camel == "setAt" {
                        normalize_system_time_keys(value)
                    } else {
                        normalize_compact_metadata_keys(value)
                    };
                    (camel, value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(normalize_compact_metadata_keys)
                .collect::<Vec<_>>(),
        ),
        scalar => scalar,
    }
}

fn normalize_system_time_keys(value: Value) -> Value {
    let Value::Object(object) = value else {
        return value;
    };
    Value::Object(
        object
            .into_iter()
            .map(|(key, value)| {
                let key = match key.as_str() {
                    "secsSinceEpoch" => "secs_since_epoch".to_string(),
                    "nanosSinceEpoch" => "nanos_since_epoch".to_string(),
                    _ => key,
                };
                (key, value)
            })
            .collect(),
    )
}

fn projected_text_block(
    projection: &Utf16JsonProjection,
    pointer: &str,
    text: &str,
) -> ContentBlock {
    let units = projection
        .string_units(pointer)
        .unwrap_or_else(|| text.encode_utf16().collect());
    if text.encode_utf16().ne(units.iter().copied()) {
        ContentBlock::TextJsUtf16 {
            text: text.to_owned(),
            utf16_code_units: units,
            citations: None,
        }
    } else {
        ContentBlock::Text {
            text: text.to_owned(),
            citations: None,
        }
    }
}

fn parse_assistant_content(projection: Option<&Utf16JsonProjection>) -> Vec<ContentBlock> {
    let Some(projection) = projection else {
        return Vec::new();
    };
    let content = &projection.value;
    if let Some(text) = content.as_str() {
        return vec![projected_text_block(projection, "", text)];
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };

    blocks
        .iter()
        .enumerate()
        .filter_map(
            |(index, block)| match block.get("type").and_then(Value::as_str)? {
                "text" => Some(projected_text_block(
                    projection,
                    &format!("/{index}/text"),
                    block.get("text")?.as_str()?,
                )),
                "thinking" => Some(ContentBlock::Thinking {
                    thinking: block.get("thinking")?.as_str()?.to_string(),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }),
                "redacted_thinking" => Some(ContentBlock::RedactedThinking {
                    data: block.get("data")?.as_str()?.to_string(),
                }),
                "tool_use" => {
                    let provider_id = block.get("id")?.as_str()?.to_string();
                    Some(ContentBlock::ToolUse {
                        id: ToolUseId::from(provider_id.clone()),
                        name: block.get("name")?.as_str()?.to_string(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                        input_projection: projection.subprojection(&format!("/{index}/input")).ok(),
                        provider_id: Some(provider_id),
                    })
                }
                "server_tool_use" => Some(ContentBlock::ServerToolUse {
                    id: block.get("id")?.as_str()?.to_string(),
                    name: block.get("name")?.as_str()?.to_string(),
                    input: block.get("input").cloned().unwrap_or(Value::Null),
                }),
                "connector_text" => Some(ContentBlock::ConnectorText {
                    connector_text: block
                        .get("connector_text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }),
                "advisor_tool_result" => Some(ContentBlock::AdvisorToolResult {
                    tool_use_id: block.get("tool_use_id")?.as_str()?.to_string(),
                    content: block.get("content").cloned().unwrap_or(Value::Null),
                    is_error: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }),
                _ => None,
            },
        )
        .collect()
}

// ── Replay-ack emitter ────────────────────────────────────────────────────────

/// Build the `user` replay-ack frame that echoes the ORIGINAL message so the
/// host can correlate it (claude-code `SDKUserMessageReplaySchema`): same
/// `uuid`, same `content`, same `timestamp` (when the inbound frame carried
/// one — else a fresh one), `isReplay:true`.
///
/// `content` is the original message content (string or content-block array).
/// `timestamp` is the original frame timestamp, if any.
fn build_replay_ack_frame(
    uuid: &str,
    content: &Value,
    timestamp: Option<&str>,
    session_id: &str,
) -> Value {
    let timestamp = timestamp.map_or_else(
        || chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ToString::to_string,
    );
    json!({
        "type": "user",
        "message": {"role": "user", "content": content},
        "session_id": session_id,
        "parent_tool_use_id": null,
        "uuid": uuid,
        "timestamp": timestamp,
        "isReplay": true
    })
}

/// Queue an already-normalized replay/history frame on the sole stdout writer.
pub fn emit_raw_frame_queued(out_tx: &OutboundTx, frame: &Value) -> Result<(), InputError> {
    out_tx
        .send(OutboundMsg::Line(serialize_ndjson_line(frame)))
        .map_err(|_| InputError::DeliveryFailed)
}

pub fn emit_projected_frame_queued(
    out_tx: &OutboundTx,
    frame: &Utf16JsonProjection,
) -> Result<(), InputError> {
    let line = crate::headless::stream_json::serialize_projected_ndjson_line(frame)
        .map_err(|_| InputError::MalformedJson)?;
    out_tx
        .send(OutboundMsg::Line(line))
        .map_err(|_| InputError::DeliveryFailed)
}

pub fn emit_replay_ack_projected_queued(
    out_tx: &OutboundTx,
    original: &Utf16JsonProjection,
    session_id: &str,
) -> Result<(), InputError> {
    let uuid = original
        .value
        .get("uuid")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let content = original
        .value
        .pointer("/message/content")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let timestamp = original.value.get("timestamp").and_then(Value::as_str);
    let mut replay = Utf16JsonProjection::plain(build_replay_ack_frame(
        uuid, &content, timestamp, session_id,
    ));
    for field in ["uuid", "timestamp"] {
        if let Ok(child) = original.subprojection(&format!("/{field}")) {
            replay
                .set_field(field, child)
                .map_err(|_| InputError::MalformedJson)?;
        }
    }
    if let Ok(content) = original.subprojection("/message/content") {
        let mut message = Utf16JsonProjection::plain(json!({"role":"user"}));
        message
            .set_field("content", content)
            .map_err(|_| InputError::MalformedJson)?;
        replay
            .set_field("message", message)
            .map_err(|_| InputError::MalformedJson)?;
    }
    emit_projected_frame_queued(out_tx, &replay)
}

/// Emit a replay-ack frame through the single-writer outbound queue.
///
/// This preserves the strict FIFO ordering the control protocol requires: the
/// ack is serialised and pushed onto the same `OutboundMsg` channel the
/// `emit_*` frame methods use, so the drain task writes it in enqueue order
/// relative to every data frame. All STREAMING replay-ack sites — the
/// in-turn-loop ack in `run.rs` and the duplicate-ack in [`spawn_stdin_router`]
/// — go through here; byte IO is always owned by the injected writer.
pub fn emit_replay_ack_queued(
    out_tx: &OutboundTx,
    uuid: &str,
    content: &Value,
    timestamp: Option<&str>,
    session_id: &str,
) -> Result<(), InputError> {
    let frame = build_replay_ack_frame(uuid, content, timestamp, session_id);
    let line = serialize_ndjson_line(&frame);
    out_tx
        .send(OutboundMsg::Line(line))
        .map_err(|_| InputError::DeliveryFailed)
}

// ── Content extractor ─────────────────────────────────────────────────────────

/// Extract a `&str` prompt from a `Value` that is either a JSON `String`
/// (the simple form) or a `Value::Array` of content blocks. In the array case
/// we join all text blocks with no separator (same as the wire content flatten
/// the TS orchestrator uses when concatenating content arrays into a `string`
/// prompt for the model). Returns the owned string.
pub fn content_to_prompt(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// ── Stdin line reader (sync, used by the async wrapper) ──────────────────────

/// Flatten text using exact JavaScript code units for provider request input.
pub fn content_to_prompt_projection(content: &Utf16JsonProjection) -> Utf16JsonProjection {
    let units = if content.value.is_string() {
        content.string_units("").unwrap_or_default()
    } else if let Some(blocks) = content.value.as_array() {
        blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.get("type").and_then(Value::as_str) == Some("text"))
            .flat_map(|(index, _)| {
                content
                    .string_units(&format!("/{index}/text"))
                    .unwrap_or_default()
            })
            .collect()
    } else {
        Vec::new()
    };
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("validated content retains display text")
}

/// Read all lines from `reader` (stdin in production), process each through
/// [`process_line`], and collect the resulting [`UserTurn`]s in order, also
/// tracking which turns should be skipped (duplicate uuids). Returns:
///
/// - `Ok(turns)` on success, where each element is `(UserTurn, is_duplicate)`.
/// - `Err(InputError)` if any line is fatally invalid.
///
/// Empty lines are skipped; a trailing line without a newline is still processed.
///
/// `replay_user_messages` controls whether duplicate-ack frames are emitted.
#[cfg(test)]
fn read_input_turns(
    reader: impl BufRead,
    replay_user_messages: bool,
    session_id: &str,
) -> Result<Vec<UserTurn>, InputError> {
    let mut seen_uuids: HashSet<String> = HashSet::new();
    let mut turns: Vec<UserTurn> = Vec::new();

    for line_result in reader.lines() {
        let line = line_result.map_err(|e| {
            let _ = e;
            InputError::ReadFailed
        })?;
        let line = line.trim_end_matches('\r'); // strip CRLF if any
        if line.trim().is_empty() {
            continue;
        }
        match process_line(line, &mut seen_uuids, &mut |_| {})? {
            FrameAction::UserTurn(turn) => {
                turns.push(turn);
            }
            FrameAction::DuplicateUser {
                uuid,
                content,
                timestamp,
                ..
            } => {
                let _ = (uuid, content, timestamp, replay_user_messages, session_id);
                // Duplicate turns are skipped — do NOT push.
            }
            // Control frames are silently dropped in the legacy batch reader
            // (used only by tests and non-streaming callers). The streaming
            // reader `spawn_stdin_router` routes them to their channels instead.
            FrameAction::ControlRequest(_)
            | FrameAction::ControlCancel(_)
            | FrameAction::ControlResponse(_)
            | FrameAction::History(_)
            | FrameAction::BashCommand(_)
            | FrameAction::UpdateEnvironmentVariables(_)
            | FrameAction::Consumed => {}
        }
    }

    Ok(turns)
}

// ── Streaming injected input router ────────────────────────────────────────
#[derive(Debug)]
pub enum StdinControlFrame {
    Request(Utf16JsonProjection),
    Cancel(Utf16JsonProjection),
    UpdateEnvironmentVariables(HashMap<String, String>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StdinReaderStatus {
    Reading,
    Eof,
    Failed(InputError),
    Stopped,
}

/// Owns cancellation and completion of the async reader. Hosts stop and join
/// this scope before returning, even if the peer holds its input open.
pub struct StdinReaderControl {
    stop: tokio_util::sync::CancellationToken,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl StdinReaderControl {
    pub fn stop(&self) {
        self.stop.cancel();
    }
    pub async fn join(&self) -> Result<(), InputError> {
        if let Some(task) = self.task.lock().await.take() {
            task.await.map_err(|_| InputError::ReadFailed)?;
        }
        Ok(())
    }
}
impl Drop for StdinReaderControl {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct StdinChannels {
    pub input_rx: mpsc::Receiver<StreamInput>,
    pub input_pending: PendingInputQueue,
    pub control_req_rx: mpsc::UnboundedReceiver<StdinControlFrame>,
    pub control_resp_rx: mpsc::UnboundedReceiver<Utf16JsonProjection>,
    pub status: tokio::sync::watch::Receiver<StdinReaderStatus>,
    pub reader: StdinReaderControl,
}

/// The same accepted FIFO waiting for bounded channel delivery. Router flush
/// and post-tool snapshots use this one lock; no item is removed in flight.
pub type PendingInputQueue = Arc<StdMutex<VecDeque<StreamInput>>>;

pub fn spawn_stdin_router(
    input: Input,
    stderr: Output,
    replay_user_messages: bool,
    session_id: String,
    out_tx: Arc<OutboundTx>,
    lifecycle: Arc<crate::headless::queued_commands::QueueLifecycle>,
) -> StdinChannels {
    spawn_stdin_router_from_reader(
        input,
        stderr,
        replay_user_messages,
        session_id,
        out_tx,
        lifecycle,
    )
}

pub(crate) fn spawn_stdin_router_from_reader<R: tokio::io::AsyncRead + Send + 'static>(
    reader: R,
    stderr: Output,
    replay_user_messages: bool,
    session_id: String,
    out_tx: Arc<OutboundTx>,
    lifecycle: Arc<crate::headless::queued_commands::QueueLifecycle>,
) -> StdinChannels {
    let (input_tx, input_rx) = mpsc::channel(64);
    // Control frames must progress while the sequential turn consumer is busy.
    let (control_req_tx, control_req_rx) = mpsc::unbounded_channel();
    let (control_resp_tx, control_resp_rx) = mpsc::unbounded_channel();
    let (status_tx, status) = tokio::sync::watch::channel(StdinReaderStatus::Reading);
    let stop = tokio_util::sync::CancellationToken::new();
    let task_stop = stop.clone();
    let input_pending = PendingInputQueue::default();
    let pending = input_pending.clone();
    let task = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(Box::pin(reader)).lines();
        let mut seen = HashSet::new();
        let mut outcome = StdinReaderStatus::Eof;
        let mut ended = false;
        'reader: loop {
            let has_pending = !pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty();
            if ended && !has_pending {
                break;
            }
            tokio::select! {
                biased;
                _ = task_stop.cancelled() => { outcome = StdinReaderStatus::Stopped; break; }
                permit = input_tx.reserve(), if has_pending => {
                    match permit {
                        Ok(permit) => {
                            let mut pending = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(input) = pending.pop_front() { permit.send(input); }
                        }
                        Err(_) => break,
                    }
                }
                next = lines.next_line(), if !ended => {
                    let line = match next {
                        Ok(Some(line)) => line,
                        Ok(None) => { ended = true; status_tx.send_replace(StdinReaderStatus::Eof); continue; }
                        Err(_) => {
                            tokio::select! {
                                _ = task_stop.cancelled() => { outcome = StdinReaderStatus::Stopped; break; }
                                _ = stderr.write_line("Error reading streaming input") => {}
                            }
                            outcome = StdinReaderStatus::Failed(InputError::ReadFailed); break;
                        }
                    };
                    let mut diagnostics = Vec::new();
                    let action = process_line(line.trim_end_matches('\r'), &mut seen, &mut |line| diagnostics.push(line.to_owned()));
                    for line in diagnostics {
                        tokio::select! {
                            _ = task_stop.cancelled() => { outcome = StdinReaderStatus::Stopped; break 'reader; }
                            _ = stderr.write_line(&line) => {}
                        }
                    }
                    match action {
                        Ok(FrameAction::UserTurn(mut turn)) => {
                            turn.queue_delivery = Some(lifecycle.input_delivery());
                            let mut pending = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            lifecycle.record_enqueued(&turn);
                            if let Some(uuid) = turn.uuid.as_deref() {
                                if lifecycle.command_queued(uuid).is_err() { outcome = StdinReaderStatus::Failed(InputError::DeliveryFailed); break; }
                            }
                            pending.push_back(StreamInput::User(turn));
                        }
                        Ok(FrameAction::History(history)) => pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push_back(StreamInput::History(history)),
                        Ok(FrameAction::BashCommand(command)) => pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push_back(StreamInput::Bash(command)),
                        Ok(FrameAction::DuplicateUser { frame_projection, .. }) => {
                            if replay_user_messages && emit_replay_ack_projected_queued(&out_tx, &frame_projection, &session_id).is_err() { outcome = StdinReaderStatus::Failed(InputError::DeliveryFailed); break; }
                        }
                        Ok(FrameAction::ControlRequest(frame)) => { let _ = control_req_tx.send(StdinControlFrame::Request(frame)); }
                        Ok(FrameAction::ControlCancel(id)) => { let _ = control_req_tx.send(StdinControlFrame::Cancel(id)); }
                        Ok(FrameAction::ControlResponse(frame)) => { let _ = control_resp_tx.send(frame); }
                        Ok(FrameAction::UpdateEnvironmentVariables(map)) => { let _ = control_req_tx.send(StdinControlFrame::UpdateEnvironmentVariables(map)); }
                        Ok(FrameAction::Consumed) => {}
                        Err(error) => { outcome = StdinReaderStatus::Failed(error); break; }
                    }
                }
            }
        }
        status_tx.send_replace(if task_stop.is_cancelled() {
            StdinReaderStatus::Stopped
        } else {
            outcome
        });
    });
    StdinChannels {
        input_rx,
        input_pending,
        control_req_rx,
        control_resp_rx,
        status,
        reader: StdinReaderControl {
            stop,
            task: tokio::sync::Mutex::new(Some(task)),
        },
    }
}

// ── Control-response frame builder ───────────────────────────────────────────

/// Build the byte-exact `control_response` error envelope.
///
/// Shape (from GROUND-TRUTH-init.md §2.2 fallthrough):
/// ```json
/// {"type":"control_response","response":{"subtype":"error","request_id":"<id>","error":"<msg>"}}
/// ```
///
/// The error string for unsupported subtypes is:
/// `"Unsupported control request subtype: <subtype>"` (binary-confirmed).
pub fn build_control_response_error(request_id: &str, error_msg: &str) -> Value {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error_msg
        }
    })
}

/// Build the byte-exact `control_response` success envelope.
///
/// Shape:
/// ```json
/// {"type":"control_response","response":{"subtype":"success","request_id":"<id>","response":{...}}}
/// ```
/// When `payload` is `None`, the `"response"` key is OMITTED (not `null`).
pub fn build_control_response_success(request_id: &str, payload: Option<Value>) -> Value {
    let mut inner = serde_json::Map::new();
    inner.insert("subtype".into(), json!("success"));
    inner.insert("request_id".into(), json!(request_id));
    if let Some(p) = payload {
        inner.insert("response".into(), p);
    }
    json!({
        "type": "control_response",
        "response": Value::Object(inner)
    })
}

/// Extract the `subtype` string from a `control_request` frame's `request` object,
/// returning `""` if absent (for the fallthrough error path).
pub fn control_request_subtype(frame: &Value) -> &str {
    frame
        .get("request")
        .and_then(|r| r.get("subtype"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Extract the `request_id` string from a `control_request` or `control_response` frame.
pub fn control_frame_request_id(frame: &Value) -> &str {
    frame
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
}

// ── ControlPlaneWriter ────────────────────────────────────────────────────────

/// Wraps the outbound NDJSON sender so the control-request dispatcher can reply
/// without holding a reference to the full `StreamJsonStream`.
///
/// One `ControlPlaneWriter` is created per `run_stream_json_input_loop` invocation
/// and moved into the control-dispatcher task. It clones the sender arc so it
/// shares the same single-writer stdout drain as the streaming output.
pub struct ControlPlaneWriter {
    tx: std::sync::Arc<crate::headless::stream_json::OutboundTx>,
}

impl ControlPlaneWriter {
    pub fn reply_error_projected(
        &self,
        request_id: Utf16JsonProjection,
        message: &str,
    ) -> Result<(), InputError> {
        let mut response = Utf16JsonProjection::plain(json!({"subtype":"error"}));
        response
            .set_field("request_id", request_id)
            .map_err(|_| InputError::MalformedJson)?;
        response
            .set_field("error", Utf16JsonProjection::plain(json!(message)))
            .map_err(|_| InputError::MalformedJson)?;
        let mut frame = Utf16JsonProjection::plain(json!({"type":"control_response"}));
        frame
            .set_field("response", response)
            .map_err(|_| InputError::MalformedJson)?;
        self.send_projected(&frame)
    }
    /// Wrap an `Arc<OutboundTx>` (obtained via `StreamJsonStream::outbound_tx()`).
    pub fn new(tx: std::sync::Arc<crate::headless::stream_json::OutboundTx>) -> Self {
        Self { tx }
    }

    pub fn send_projected(&self, frame: &Utf16JsonProjection) -> Result<(), InputError> {
        emit_projected_frame_queued(&self.tx, frame)
    }
    pub fn reply_success_projected(
        &self,
        request_id: Utf16JsonProjection,
        payload: Option<Utf16JsonProjection>,
    ) -> Result<(), InputError> {
        let mut response = Utf16JsonProjection::plain(json!({"subtype":"success"}));
        response
            .set_field("request_id", request_id)
            .map_err(|_| InputError::MalformedJson)?;
        if let Some(payload) = payload {
            response
                .set_field("response", payload)
                .map_err(|_| InputError::MalformedJson)?;
        }
        let mut frame = Utf16JsonProjection::plain(json!({"type":"control_response"}));
        frame
            .set_field("response", response)
            .map_err(|_| InputError::MalformedJson)?;
        self.send_projected(&frame)
    }

    /// Send a success `control_response` envelope.
    ///
    /// When `payload` is `None` the inner `"response"` key is omitted (not `null`).
    pub fn reply_success(
        &self,
        request_id: &str,
        payload: Option<serde_json::Value>,
    ) -> Result<(), InputError> {
        let frame = build_control_response_success(request_id, payload);
        let line = crate::headless::stream_json::serialize_ndjson_line(&frame);
        self.tx
            .send(crate::headless::stream_json::OutboundMsg::Line(line))
            .map_err(|_| InputError::DeliveryFailed)
    }

    /// Send an error `control_response` envelope.
    pub fn reply_error(&self, request_id: &str, msg: &str) -> Result<(), InputError> {
        let frame = build_control_response_error(request_id, msg);
        let line = crate::headless::stream_json::serialize_ndjson_line(&frame);
        self.tx
            .send(crate::headless::stream_json::OutboundMsg::Line(line))
            .map_err(|_| InputError::DeliveryFailed)
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn process_line(line: &str, seen: &mut HashSet<String>) -> Result<FrameAction, InputError> {
        super::process_line(line, seen, &mut |_| {})
    }

    fn fresh_seen() -> HashSet<String> {
        HashSet::new()
    }

    fn outbound_line(msg: crate::headless::stream_json::OutboundMsg) -> String {
        match msg {
            crate::headless::stream_json::OutboundMsg::Line(line) => line,
            crate::headless::stream_json::OutboundMsg::StreamEvent(line) => line,
            crate::headless::stream_json::OutboundMsg::Heartbeats(_) => {
                panic!("unexpected heartbeat message")
            }
            crate::headless::stream_json::OutboundMsg::Shutdown(_)
            | crate::headless::stream_json::OutboundMsg::PublishJson(_)
            | crate::headless::stream_json::OutboundMsg::RefreshHeldResultTotals(_)
            | crate::headless::stream_json::OutboundMsg::Flush(_) => {
                panic!("unexpected flush message")
            }
        }
    }

    // ── normalizeControlMessageKeys ──────────────────────────────────────────

    #[test]
    fn normalize_top_level_request_id() {
        let mut v = json!({"requestId": "abc", "type": "user"});
        normalize_control_message_keys(&mut v);
        assert!(
            v.get("request_id").is_some(),
            "requestId should become request_id"
        );
        assert!(v.get("requestId").is_none(), "requestId should be removed");
    }

    #[test]
    fn normalize_nested_response_request_id() {
        let mut v =
            json!({"type": "control_response", "response": {"requestId": "xyz", "data": 1}});
        normalize_control_message_keys(&mut v);
        let resp = v.get("response").unwrap().as_object().unwrap();
        assert!(
            resp.contains_key("request_id"),
            "response.requestId should become request_id"
        );
        assert!(!resp.contains_key("requestId"));
    }

    #[test]
    fn normalize_leaves_snake_case_unchanged() {
        let mut v = json!({"request_id": "abc", "type": "user"});
        normalize_control_message_keys(&mut v);
        assert_eq!(v["request_id"], "abc");
    }

    // ── keep_alive ───────────────────────────────────────────────────────────

    #[test]
    fn keep_alive_is_consumed() {
        let line = r#"{"type":"keep_alive"}"#;
        let result = process_line(line, &mut fresh_seen()).unwrap();
        assert!(matches!(result, FrameAction::Consumed));
    }

    // ── unknown type ─────────────────────────────────────────────────────────

    #[test]
    fn unknown_type_is_consumed_with_warning() {
        let line = r#"{"type":"__warp_speed__"}"#;
        let result = process_line(line, &mut fresh_seen()).unwrap();
        assert!(matches!(result, FrameAction::Consumed));
    }

    // ── malformed JSON ───────────────────────────────────────────────────────

    #[test]
    fn malformed_json_is_fatal() {
        let line = r#"{not valid json}"#;
        let err = process_line(line, &mut fresh_seen()).unwrap_err();
        assert_eq!(err, InputError::MalformedJson);
    }

    // ── user frame ───────────────────────────────────────────────────────────

    #[test]
    fn user_frame_with_string_content_is_extracted() {
        let line = r#"{"type":"user","message":{"role":"user","content":"hello world"},"parent_tool_use_id":null}"#;
        let result = process_line(line, &mut fresh_seen()).unwrap();
        match result {
            FrameAction::UserTurn(turn) => {
                assert_eq!(turn.content, Value::String("hello world".to_string()));
                assert!(turn.uuid.is_none());
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }

    #[test]
    fn user_frame_with_content_block_array_is_extracted() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"from block"}]},"parent_tool_use_id":null}"#;
        let result = process_line(line, &mut fresh_seen()).unwrap();
        match result {
            FrameAction::UserTurn(turn) => {
                assert!(turn.content.is_array(), "content should be an array");
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }

    #[test]
    fn user_frame_bad_role_is_fatal() {
        let line = r#"{"type":"user","message":{"role":"assistant","content":"bad"},"parent_tool_use_id":null}"#;
        let err = process_line(line, &mut fresh_seen()).unwrap_err();
        match err {
            InputError::BadRole(got) => assert_eq!(got, "assistant"),
            other => panic!("expected BadRole, got {other:?}"),
        }
    }

    #[test]
    fn user_frame_bad_role_error_string() {
        // Verify the exact error message format.
        let err = InputError::BadRole("assistant".to_string());
        assert_eq!(err.to_string(), "Error: Expected message role 'user'");
    }

    #[test]
    fn assistant_history_preserves_protected_and_provider_tool_blocks() {
        let line = r#"{"type":"assistant","uuid":"11111111-1111-1111-1111-111111111111","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"text","text":"hello"},{"type":"thinking","thinking":"why","signature":"sig"},{"type":"redacted_thinking","data":"opaque"},{"type":"tool_use","id":"toolu_provider","name":"Read","input":{"file_path":"a"}}]}}"#;
        let action = process_line(line, &mut fresh_seen()).unwrap();
        let FrameAction::History(history) = action else {
            panic!("expected history action");
        };
        assert!(history.replay_frame.is_some());
        let ConversationMessage::Assistant {
            content,
            stop_reason,
            ..
        } = history.message
        else {
            panic!("expected assistant message");
        };
        assert_eq!(stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(content.len(), 4);
        assert!(matches!(
            &content[3],
            ContentBlock::ToolUse { id, provider_id: Some(provider_id), .. }
                if id.as_str() == "toolu_provider" && provider_id == "toolu_provider"
        ));
    }

    #[test]
    fn compact_boundary_history_and_legacy_bash_command_are_routed() {
        let system = process_line(
            r#"{"type":"system","subtype":"compact_boundary","uuid":"11111111-1111-1111-1111-111111111111","compact_metadata":{"trigger":"manual","pre_tokens":42}}"#,
            &mut fresh_seen(),
        )
        .unwrap();
        assert!(matches!(
            system,
            FrameAction::History(HistoryInput {
                message: ConversationMessage::System {
                    content,
                    subtype: Some(subtype),
                    compact_metadata: Some(metadata),
                    ..
                },
                replay_frame: None,
                ..
            }) if content == "Conversation compacted"
                && subtype == "compact_boundary"
                && metadata.trigger == lingxi_core::types::CompactTrigger::Manual
                && metadata.pre_tokens == 42
        ));

        let bash = process_line(
            r#"{"type":"bash_command","command":"printf hello"}"#,
            &mut fresh_seen(),
        )
        .unwrap();
        assert!(matches!(
            bash,
            FrameAction::BashCommand(BashCommand { command }) if command == "printf hello"
        ));
    }

    #[test]
    fn compact_boundary_replay_preserves_parent_and_optional_261_metadata() {
        let action = process_line(
            r#"{"type":"system","subtype":"compact_boundary","uuid":"11111111-1111-1111-1111-111111111111","logical_parent_uuid":"parent","compact_metadata":{"trigger":"manual","pre_tokens":42,"precomputed":true,"preserved_messages":{"anchor_uuid":"summary","uuids":["tail"]}}}"#,
            &mut fresh_seen(),
        ).unwrap();
        let FrameAction::History(HistoryInput {
            message:
                ConversationMessage::System {
                    compact_metadata: Some(metadata),
                    ..
                },
            ..
        }) = action
        else {
            panic!("expected typed compact boundary");
        };
        assert_eq!(metadata.logical_parent_uuid.as_deref(), Some("parent"));
        assert_eq!(metadata.precomputed, Some(true));
        let preserved = metadata.preserved_messages.unwrap();
        assert_eq!(preserved.uuids, ["tail"]);
        assert!(preserved.all_uuids.is_empty());
        assert!(serde_json::to_value(preserved)
            .unwrap()
            .get("allUuids")
            .is_none());
    }

    #[test]
    fn compact_boundary_history_preserves_camelized_active_goal() {
        let action = process_line(
            r#"{"type":"system","subtype":"compact_boundary","uuid":"11111111-1111-1111-1111-111111111111","compact_metadata":{"trigger":"manual","active_goal":{"condition":"finish","set_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0},"last_reason":"working"}}}"#,
            &mut fresh_seen(),
        )
        .unwrap();
        let FrameAction::History(HistoryInput {
            message:
                ConversationMessage::System {
                    compact_metadata: Some(metadata),
                    ..
                },
            ..
        }) = action
        else {
            panic!("expected typed compact boundary");
        };
        let goal = metadata.active_goal.expect("active goal");
        assert_eq!(goal.condition, "finish");
        assert_eq!(goal.last_reason.as_deref(), Some("working"));
    }

    #[test]
    fn informational_system_history_is_ignored() {
        let action = process_line(
            r#"{"type":"system","subtype":"informational","content":"do not inject"}"#,
            &mut fresh_seen(),
        )
        .unwrap();
        assert!(matches!(action, FrameAction::Consumed));
    }

    // ── UUID dedup ───────────────────────────────────────────────────────────

    #[test]
    fn duplicate_uuid_returns_duplicate_action() {
        let uuid = "11111111-1111-1111-1111-111111111111";
        let line = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"hi"}},"parent_tool_use_id":null,"uuid":"{uuid}"}}"#
        );
        let mut seen = fresh_seen();
        // First occurrence → UserTurn.
        let first = process_line(&line, &mut seen).unwrap();
        assert!(matches!(first, FrameAction::UserTurn(_)));
        // Second occurrence → DuplicateUser.
        let second = process_line(&line, &mut seen).unwrap();
        match second {
            FrameAction::DuplicateUser {
                uuid: u, content, ..
            } => {
                assert_eq!(u, uuid);
                // The ack must echo the ORIGINAL content, not an empty string.
                assert_eq!(content, serde_json::json!("hi"));
            }
            other => panic!("expected DuplicateUser, got {other:?}"),
        }
    }

    #[test]
    fn different_uuids_both_accepted() {
        let uuid_a = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let uuid_b = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let line_a = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"a"}},"parent_tool_use_id":null,"uuid":"{uuid_a}"}}"#
        );
        let line_b = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"b"}},"parent_tool_use_id":null,"uuid":"{uuid_b}"}}"#
        );
        let mut seen = fresh_seen();
        assert!(matches!(
            process_line(&line_a, &mut seen).unwrap(),
            FrameAction::UserTurn(_)
        ));
        assert!(matches!(
            process_line(&line_b, &mut seen).unwrap(),
            FrameAction::UserTurn(_)
        ));
    }

    // ── control_request ──────────────────────────────────────────────────────

    #[test]
    fn control_request_without_request_field_is_fatal() {
        let line = r#"{"type":"control_request"}"#;
        let err = process_line(line, &mut fresh_seen()).unwrap_err();
        assert_eq!(err, InputError::MissingRequest);
    }

    #[test]
    fn control_request_with_request_field_routes_to_control_request() {
        // Phase 0: control_request frames are now routed to ControlRequest(frame)
        // rather than silently consumed. The caller's dispatcher stub sends the
        // byte-exact "Unsupported control request subtype: <subtype>" error.
        let line = r#"{"type":"control_request","request":{"subtype":"get_status"}}"#;
        let result = process_line(line, &mut fresh_seen()).unwrap();
        assert!(matches!(result, FrameAction::ControlRequest(_)));
        // Verify the frame carries the request field.
        if let FrameAction::ControlRequest(frame) = result {
            assert!(
                frame.value.get("request").is_some(),
                "ControlRequest frame must carry the request field"
            );
        }
    }

    #[test]
    fn control_cancel_request_routes_to_control_cancel() {
        let line = r#"{"type":"control_cancel_request","request_id":"req-cancel-1"}"#;
        let result = process_line(line, &mut fresh_seen()).unwrap();
        match result {
            FrameAction::ControlCancel(request_id) => {
                assert_eq!(request_id.value, "req-cancel-1");
            }
            _ => panic!("expected ControlCancel"),
        }
    }

    #[test]
    fn missing_request_error_string() {
        assert_eq!(
            InputError::MissingRequest.to_string(),
            "Error: Missing request on control_request"
        );
    }

    // ── Phase 0: control_response_error builder ───────────────────────────────

    #[test]
    fn build_control_response_error_has_correct_shape() {
        let resp = build_control_response_error(
            "req_abc",
            "Unsupported control request subtype: get_status",
        );
        assert_eq!(resp["type"], "control_response");
        let inner = &resp["response"];
        assert_eq!(inner["subtype"], "error");
        assert_eq!(inner["request_id"], "req_abc");
        assert_eq!(
            inner["error"],
            "Unsupported control request subtype: get_status"
        );
    }

    #[test]
    fn build_control_response_success_with_payload() {
        let payload = json!({"pid": 42});
        let resp = build_control_response_success("req_xyz", Some(payload.clone()));
        assert_eq!(resp["type"], "control_response");
        let inner = &resp["response"];
        assert_eq!(inner["subtype"], "success");
        assert_eq!(inner["request_id"], "req_xyz");
        assert_eq!(inner["response"], payload);
    }

    #[test]
    fn build_control_response_success_without_payload_omits_response_key() {
        let resp = build_control_response_success("req_xyz", None);
        let inner = &resp["response"];
        // When no payload, the "response" key must be ABSENT (not null).
        assert!(
            inner.get("response").is_none(),
            "response key must be absent when payload is None"
        );
    }

    #[test]
    fn control_request_subtype_extracts_subtype() {
        let frame = json!({"type": "control_request", "request_id": "r1", "request": {"subtype": "initialize"}});
        assert_eq!(control_request_subtype(&frame), "initialize");
    }

    #[test]
    fn control_frame_request_id_extracts_id() {
        let frame = json!({"type": "control_request", "request_id": "req-123", "request": {"subtype": "x"}});
        assert_eq!(control_frame_request_id(&frame), "req-123");
    }

    // ── Phase 0: control_response frame routes to ControlResponse variant ─────

    #[test]
    fn control_response_routes_to_control_response_variant() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r1","response":{}}}"#;
        let result = process_line(line, &mut fresh_seen()).unwrap();
        assert!(matches!(result, FrameAction::ControlResponse(_)));
    }

    // ── content_to_prompt ────────────────────────────────────────────────────

    #[test]
    fn content_to_prompt_string_passthrough() {
        let v = Value::String("hello".to_string());
        assert_eq!(content_to_prompt(&v), "hello");
    }

    #[test]
    fn content_to_prompt_block_array_joins_text_blocks() {
        let v = json!([
            {"type": "text", "text": "hello "},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(content_to_prompt(&v), "hello world");
    }

    #[test]
    fn content_to_prompt_skips_non_text_blocks() {
        let v = json!([
            {"type": "image", "source": {}},
            {"type": "text", "text": "only this"}
        ]);
        assert_eq!(content_to_prompt(&v), "only this");
    }

    #[test]
    fn content_to_prompt_null_returns_empty() {
        let v = Value::Null;
        assert_eq!(content_to_prompt(&v), "");
    }

    // ── read_input_turns (multi-line) ─────────────────────────────────────────

    #[test]
    fn read_input_turns_processes_three_lines_in_order() {
        let input = r#"{"type":"keep_alive"}
{"type":"user","message":{"role":"user","content":"first"},"parent_tool_use_id":null}
{"type":"user","message":{"role":"user","content":"second"},"parent_tool_use_id":null}
{"type":"user","message":{"role":"user","content":"third"},"parent_tool_use_id":null}
"#;
        let turns =
            read_input_turns(io::BufReader::new(input.as_bytes()), false, "sess-id").unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].content, Value::String("first".to_string()));
        assert_eq!(turns[1].content, Value::String("second".to_string()));
        assert_eq!(turns[2].content, Value::String("third".to_string()));
    }

    #[test]
    fn read_input_turns_skips_empty_lines() {
        let input = "\n\n{\"type\":\"keep_alive\"}\n\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"x\"},\"parent_tool_use_id\":null}\n";
        let turns = read_input_turns(io::BufReader::new(input.as_bytes()), false, "sess").unwrap();
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn read_input_turns_deduplicates_same_uuid() {
        let uuid = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let line = format!(
            "{}\n{}\n",
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":"a"}},"parent_tool_use_id":null,"uuid":"{uuid}"}}"#
            ),
            format!(
                r#"{{"type":"user","message":{{"role":"user","content":"b"}},"parent_tool_use_id":null,"uuid":"{uuid}"}}"#
            )
        );
        // Without replay (no ack emitted to stdout in tests).
        let turns = read_input_turns(io::BufReader::new(line.as_bytes()), false, "sess").unwrap();
        // Only the first occurrence should be in the turn list.
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, Value::String("a".to_string()));
    }

    #[test]
    fn read_input_turns_malformed_line_propagates_error() {
        let input = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"ok\"},\"parent_tool_use_id\":null}\n{bad json}\n";
        let err =
            read_input_turns(io::BufReader::new(input.as_bytes()), false, "sess").unwrap_err();
        assert_eq!(err, InputError::MalformedJson);
    }

    #[test]
    fn read_input_turns_bad_role_propagates_error() {
        let input = "{\"type\":\"user\",\"message\":{\"role\":\"assistant\",\"content\":\"x\"},\"parent_tool_use_id\":null}\n";
        let err =
            read_input_turns(io::BufReader::new(input.as_bytes()), false, "sess").unwrap_err();
        assert!(matches!(err, InputError::BadRole(_)));
    }

    // ── update_environment_variables ─────────────────────────────────────────

    #[test]
    fn update_env_vars_routes_only_allowlisted_keys() {
        let action = process_line(r#"{"type":"update_environment_variables","variables":{"CLAUDE_CODE_SESSION_ACCESS_TOKEN":"tok","BASH_ENV":"evil"}}"#, &mut fresh_seen()).unwrap();
        let FrameAction::UpdateEnvironmentVariables(updates) = action else {
            panic!("host update required")
        };
        assert_eq!(
            updates
                .get("CLAUDE_CODE_SESSION_ACCESS_TOKEN")
                .map(String::as_str),
            Some("tok")
        );
        assert_eq!(updates.len(), 1);
    }
    #[test]
    fn update_env_vars_drops_frame_with_non_string_value() {
        assert!(matches!(process_line(r#"{"type":"update_environment_variables","variables":{"CLAUDE_CODE_SESSION_ACCESS_TOKEN":123}}"#, &mut fresh_seen()).unwrap(), FrameAction::Consumed));
    }
    #[test]
    fn update_env_vars_ignores_legacy_env_field() {
        assert!(matches!(process_line(r#"{"type":"update_environment_variables","env":{"CLAUDE_CODE_SESSION_ACCESS_TOKEN":"bad"}}"#, &mut fresh_seen()).unwrap(), FrameAction::Consumed));
    }

    #[test]
    fn whitespace_only_line_is_consumed() {
        let result = process_line("   \t", &mut fresh_seen()).unwrap();
        assert!(matches!(result, FrameAction::Consumed));
    }

    // ── Phase 1: ControlPlaneWriter ───────────────────────────────────────────

    #[test]
    fn control_plane_writer_reply_success_envelope_shape() {
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::headless::stream_json::OutboundMsg>();
        let writer = ControlPlaneWriter::new(std::sync::Arc::new(tx));
        writer.reply_success("req-1", Some(json!({"pid": 42})));

        let line = outbound_line(rx.try_recv().expect("should have sent one line"));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "control_response");
        let inner = &parsed["response"];
        assert_eq!(inner["subtype"], "success");
        assert_eq!(inner["request_id"], "req-1");
        assert_eq!(inner["response"]["pid"], 42);
    }

    #[test]
    fn control_plane_writer_reply_error_envelope_shape() {
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::headless::stream_json::OutboundMsg>();
        let writer = ControlPlaneWriter::new(std::sync::Arc::new(tx));
        writer.reply_error("req-2", "Unsupported control request subtype: foo");

        let line = outbound_line(rx.try_recv().expect("should have sent one line"));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "control_response");
        let inner = &parsed["response"];
        assert_eq!(inner["subtype"], "error");
        assert_eq!(inner["request_id"], "req-2");
        assert_eq!(inner["error"], "Unsupported control request subtype: foo");
    }

    #[test]
    fn control_plane_writer_reply_success_no_payload_omits_response_key() {
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::headless::stream_json::OutboundMsg>();
        let writer = ControlPlaneWriter::new(std::sync::Arc::new(tx));
        writer.reply_success("req-3", None);

        let line = outbound_line(rx.try_recv().expect("should have sent one line"));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        let inner = &parsed["response"];
        assert_eq!(inner["subtype"], "success");
        assert_eq!(inner["request_id"], "req-3");
        // The inner "response" key must be absent when payload is None.
        assert!(
            inner.get("response").is_none(),
            "response key must be absent when payload is None"
        );
    }

    // ── replay-ack single-writer routing (M-07) ──────────────────────────────

    #[test]
    fn queued_replay_ack_enqueues_line_not_direct_write() {
        // Streaming callers must route the replay-ack through the outbound queue
        // so it stays FIFO-ordered behind data frames (control plane never
        // overtakes the data plane). Verify the ack lands on the channel as a
        // Line with the correct frame shape.
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::headless::stream_json::OutboundMsg>();
        emit_replay_ack_queued(
            &tx,
            "uuid-1",
            &json!("hello"),
            Some("2026-07-19T00:00:00.000Z"),
            "sess-1",
        );
        let line = outbound_line(rx.try_recv().expect("ack must be enqueued as one line"));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["uuid"], "uuid-1");
        assert_eq!(parsed["isReplay"], true);
        assert_eq!(parsed["message"]["content"], "hello");
        assert_eq!(parsed["session_id"], "sess-1");
        assert_eq!(parsed["timestamp"], "2026-07-19T00:00:00.000Z");
        assert!(rx.try_recv().is_err(), "exactly one frame enqueued");
    }

    #[test]
    fn queued_and_direct_replay_ack_produce_identical_bytes() {
        // The channel-routed and direct-write paths must emit byte-identical
        // frames (same serializer, same escaping, trailing LF) so the ordering
        // fix changes only *when* the bytes hit stdout, never *what* is written.
        let (tx, mut rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::headless::stream_json::OutboundMsg>();
        emit_replay_ack_queued(
            &tx,
            "u",
            &json!([{"type": "text", "text": "hi"}]),
            Some("2026-07-19T12:00:00.000Z"),
            "s",
        );
        let queued = outbound_line(rx.try_recv().unwrap());
        let expected = serialize_ndjson_line(&build_replay_ack_frame(
            "u",
            &json!([{"type": "text", "text": "hi"}]),
            Some("2026-07-19T12:00:00.000Z"),
            "s",
        ));
        assert_eq!(queued, expected);
        assert!(queued.ends_with('\n'), "line is newline-terminated");
    }
    #[test]
    fn projected_control_reply_preserves_identity_order_and_payload() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let writer = ControlPlaneWriter::new(Arc::new(tx));
        writer
            .reply_error_projected(
                Utf16JsonProjection::parse(r#""\ud800""#).unwrap(),
                "unsupported",
            )
            .unwrap();
        assert_eq!(outbound_line(rx.try_recv().unwrap()), "{\"type\":\"control_response\",\"response\":{\"subtype\":\"error\",\"request_id\":\"\\ud800\",\"error\":\"unsupported\"}}\n");
        writer
            .reply_success_projected(
                Utf16JsonProjection::plain(json!("id")),
                Some(Utf16JsonProjection::parse(r#"{"\udfff":"\ud800"}"#).unwrap()),
            )
            .unwrap();
        assert_eq!(outbound_line(rx.try_recv().unwrap()), "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"id\",\"response\":{\"\\udfff\":\"\\ud800\"}}}\n");
    }

    #[test]
    fn control_key_normalization_rebases_exact_identity() {
        let FrameAction::ControlRequest(frame) = process_line(r#"{"type":"control_request","requestId":"\ud800","request":{"subtype":"x","\udfff":"\ud801"}}"#, &mut fresh_seen()).unwrap() else { panic!("request") };
        assert_eq!(frame.string_units("/request_id"), Some(vec![0xd800]));
        assert!(frame
            .to_json_string()
            .unwrap()
            .contains(r#""request_id":"\ud800""#));
        assert!(frame
            .subprojection("/request")
            .unwrap()
            .to_json_string()
            .unwrap()
            .contains(r#""\udfff":"\ud801""#));
    }

    #[test]
    fn user_replay_and_prompt_retain_exact_surrogates() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"a\ud800"},{"type":"text","text":"\udfff"}]},"uuid":"u","timestamp":"t"}"#;
        let FrameAction::UserTurn(turn) = process_line(line, &mut fresh_seen()).unwrap() else {
            panic!("turn")
        };
        assert_eq!(
            content_to_prompt_projection(&turn.content_projection)
                .to_json_string()
                .unwrap(),
            "\"a\u{103ff}\""
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        emit_replay_ack_projected_queued(&tx, &turn.frame_projection, "sess").unwrap();
        assert_eq!(outbound_line(rx.try_recv().unwrap()), "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"a\\ud800\"},{\"type\":\"text\",\"text\":\"\\udfff\"}]},\"session_id\":\"sess\",\"parent_tool_use_id\":null,\"uuid\":\"u\",\"timestamp\":\"t\",\"isReplay\":true}\n");
    }

    #[test]
    fn history_retains_projected_tool_input_keys() {
        let FrameAction::History(history) = process_line(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_x","name":"Bash","input":{"\ud800":"\udfff"}}]}}"#, &mut fresh_seen()).unwrap() else { panic!("history") };
        assert_eq!(
            history
                .frame_projection
                .subprojection("/message/content/0/input")
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"\ud800":"\udfff"}"#
        );
    }

    #[tokio::test]
    async fn open_input_is_cancelled_and_joined() {
        let (reader, _held_peer) = tokio::io::duplex(32);
        let (tx, _rx) = mpsc::unbounded_channel();
        let tx = Arc::new(tx);
        let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
            tx.clone(),
            "sess".into(),
        ));
        let channels = spawn_stdin_router_from_reader(
            reader,
            Output::new(tokio::io::sink()),
            false,
            "sess".into(),
            tx,
            lifecycle,
        );
        channels.reader.stop();
        tokio::time::timeout(std::time::Duration::from_secs(1), channels.reader.join())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*channels.status.borrow(), StdinReaderStatus::Stopped);
    }

    #[tokio::test]
    async fn control_progresses_past_full_turn_channel_and_eof_retains_order() {
        let mut bytes = String::new();
        for index in 0..100 {
            bytes.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{index}\"}}}}\n"
            ));
        }
        bytes.push_str("{\"type\":\"control_request\",\"request_id\":\"interrupt\",\"request\":{\"subtype\":\"interrupt\"}}\n");
        let (tx, _rx) = mpsc::unbounded_channel();
        let tx = Arc::new(tx);
        let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
            tx.clone(),
            "sess".into(),
        ));
        let mut channels = spawn_stdin_router_from_reader(
            std::io::Cursor::new(bytes.into_bytes()),
            Output::new(tokio::io::sink()),
            false,
            "sess".into(),
            tx,
            lifecycle,
        );
        let control = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            channels.control_req_rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(control, StdinControlFrame::Request(frame) if frame.value["request_id"] == "interrupt")
        );
        for index in 0..100 {
            let Some(StreamInput::User(turn)) = channels.input_rx.recv().await else {
                panic!("ordered user")
            };
            assert_eq!(turn.content, index.to_string());
        }
        assert!(channels.input_rx.recv().await.is_none());
        channels.reader.join().await.unwrap();
        assert_eq!(*channels.status.borrow(), StdinReaderStatus::Eof);
    }
}
