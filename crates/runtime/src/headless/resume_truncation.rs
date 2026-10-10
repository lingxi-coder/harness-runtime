//! (CLI-13, cc 2.1.238) The truncating resume: `--resume-session-at <message id>`
//! and its guard flag `--resume-drops-turn <message id>`.
//!
//! Oracle, inside the print-mode transcript load (@307370121):
//!
//! ```js
//! if(r.resumeSessionAt){
//!   let d=u.messages.findIndex((p)=>p.uuid===r.resumeSessionAt);
//!   if(d<0)return N("tengu_session_resumed",{…failure_reason:xe("processing_error")}),
//!     XKt(`No message found with message.uuid of: ${r.resumeSessionAt}`,r.outputFormat),fp(1),{messages:[]};
//!   if(r.resumeDropsTurn!==void 0){
//!     let p=AEy(u.messages.slice(d+1),r.resumeDropsTurn);
//!     if(!p.ok)return N("tengu_session_resumed",{…failure_reason:xe("drop_guard_refused")}),
//!       XKt(`${EEy} resuming at ${r.resumeSessionAt} would discard entries not attributable to turn ${r.resumeDropsTurn}: ${p.reason}`,r.outputFormat),fp(1),{messages:[]}}
//!   u.messages=d>=0?u.messages.slice(0,d+1):[]}
//! ```
//!
//! `--resume-session-at` names ANY chain entry, not just an assistant message —
//! that is precisely the 2.1.238 help-text reword ("the chain entry with
//! <message.id> — any chain-entry UUID, typically the kept turn's last entry").
//!
//! The guard `AEy` (@306799802) is the interesting half. It answers: "does the
//! range this resume would DISCARD consist of exactly the declared turn?" — so
//! a caller cannot silently drop absorbed queued messages, task notifications,
//! a compaction summary or content from a different turn while claiming to drop
//! one turn.
//!
//! Every field it reads (`isMeta`, `isCompactSummary`, `stackedExpansion`,
//! `promptSource`, `origin`, `attachment`) rides `JsonlMessage::extra`, the
//! flatten channel that preserves unmodelled outer fields verbatim, so this
//! port needs no schema change.

use serde_json::Value;
use session::jsonl::JsonlMessage;

/// Oracle `EEy` — the refusal prefix.
pub const DROP_GUARD_REFUSED_PREFIX: &str = "Resume rejected by --resume-drops-turn:";

/// Oracle `R9`.
const INTERRUPTED_BY_USER: &str = "[Request interrupted by user]";
/// Oracle `NA`.
const INTERRUPTED_FOR_TOOL_USE: &str = "[Request interrupted by user for tool use]";
/// Oracle `zU`.
const STOP_AND_WAIT: &str = "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed.";
/// Oracle `VY`.
const TOOL_USE_REJECTED: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";
/// Oracle `z1n` — the memory-nudge suffix appended to a rejection body.
const MEMORY_NUDGE_SUFFIX: &str = "\n\nNote: The user's next message may contain a correction or preference. Pay close attention \u{2014} if they explain what went wrong or how they'd prefer you to work, consider saving that to memory for future sessions.";

/// Oracle `Fzd = [R9, NA, zU]` — the interrupt/abort prefixes `g8` matches.
const INTERRUPT_PREFIXES: &[&str] = &[INTERRUPTED_BY_USER, INTERRUPTED_FOR_TOOL_USE, STOP_AND_WAIT];

/// Oracle `CEy` — the "furniture" attachment types a discarded range may
/// contain. Verbatim, in the oracle's own order (@306801572).
const FURNITURE_ATTACHMENTS: &[&str] = &[
    "agent_listing_delta",
    "agent_mention",
    "peer_mention",
    "already_read_file",
    "attention_budget",
    "audio_transcript",
    "auto_mode",
    "auto_mode_exit",
    "budget_usd",
    "command_permissions",
    "compact_file_reference",
    "context_efficiency",
    "critical_system_reminder",
    "date_change",
    "deferred_tools_delta",
    "diagnostics",
    "directory",
    "dynamic_skill",
    "edited_image_file",
    "edited_text_file",
    "file",
    "goal_status",
    "hook_additional_context",
    "hook_blocking_error",
    "hook_cancelled",
    "hook_deferred_tool",
    "hook_error_during_execution",
    "hook_non_blocking_error",
    "hook_permission_decision",
    "hook_plugin_listing",
    "hook_stopped_continuation",
    "hook_success",
    "hook_system_message",
    "invoked_skills",
    "max_turns_reached",
    "mcp_instructions_delta",
    "mcp_dropped_tools_delta",
    "mcp_resource",
    "memory_update",
    "nested_memory",
    "opened_file_in_ide",
    "output_style",
    "output_token_usage",
    "pdf_reference",
    "plan_file_reference",
    "plan_mode",
    "plan_mode_exit",
    "plan_mode_reentry",
    "proactivity",
    "read_truncation_notice",
    "bash_output_audience_note",
    "relevant_memories",
    "selected_lines_in_diff",
    "selected_lines_in_ide",
    "silent_turn_reminder",
    "skill_listing",
    "structured_output",
    "task_reminder",
    "team_context",
    "teammate_shutdown_batch",
    "todo_reminder",
    "token_usage",
    "tool_search_usage_reminder",
    "total_tokens_reminder",
    "batching_reminder",
    "batching_reminder_sent",
    "ultra_effort_enter",
    "ultra_effort_exit",
    "ultrathink_effort",
    "workflow_keyword_request",
    "workflow_size_guideline_change",
];

/// Oracle `qI0` — furniture types that may nevertheless NOT be skipped as
/// leading noise before the declared prompt (`WI0` subtracts them).
const NON_SKIPPABLE_FURNITURE: &[&str] = &["mcp_resource", "structured_output"];

/// Oracle `YI0` — the declared turn id must be a lowercase-or-uppercase UUID.
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

fn extra_bool(m: &JsonlMessage, key: &str) -> bool {
    m.extra.get(key).and_then(Value::as_bool) == Some(true)
}

fn content(m: &JsonlMessage) -> Option<&Value> {
    m.message.get("content")
}

/// Oracle `e.attachment.type` for an `attachment` entry.
fn attachment_type(m: &JsonlMessage) -> Option<&str> {
    m.extra
        .get("attachment")
        .and_then(|a| a.get("type"))
        .and_then(Value::as_str)
}

/// Oracle `rct(e,t)` — the entry pointer interpolated into every reason.
fn describe(m: &JsonlMessage, index: usize) -> String {
    let suffix = if m.message_type == "attachment" {
        format!(" ({})", attachment_type(m).unwrap_or_default())
    } else {
        String::new()
    };
    format!(
        "entry {index} [type={}{suffix}, uuid={}]",
        m.message_type, m.uuid
    )
}

/// Oracle `pnu(e){return e.origin===void 0||e.origin.kind==="human"||e.origin.kind==="auto-continuation"}`
fn is_locally_sourced(m: &JsonlMessage) -> bool {
    match m.extra.get("origin") {
        None | Some(Value::Null) => true,
        Some(origin) => matches!(
            origin.get("kind").and_then(Value::as_str),
            Some("human" | "auto-continuation")
        ),
    }
}

/// Oracle `kEy(e)` — the content is exactly the interrupt sentinel, either as a
/// bare string or as a single `text` block.
fn is_interrupt_sentinel(m: &JsonlMessage) -> bool {
    let Some(c) = content(m) else { return false };
    if let Some(text) = c.as_str() {
        return text == INTERRUPTED_BY_USER || text == INTERRUPTED_FOR_TOOL_USE;
    }
    let Some(arr) = c.as_array() else {
        return false;
    };
    if arr.len() != 1 {
        return false;
    }
    let b = &arr[0];
    b.get("type").and_then(Value::as_str) == Some("text")
        && matches!(
            b.get("text").and_then(Value::as_str),
            Some(INTERRUPTED_BY_USER | INTERRUPTED_FOR_TOOL_USE)
        )
}

/// Oracle `VI0 = new Set([R9, NA, zU, zU+z1n, VY, VY+z1n])`.
fn is_rejection_body(s: &str) -> bool {
    s == INTERRUPTED_BY_USER
        || s == INTERRUPTED_FOR_TOOL_USE
        || s == STOP_AND_WAIT
        || s == TOOL_USE_REJECTED
        || (s.len() == STOP_AND_WAIT.len() + MEMORY_NUDGE_SUFFIX.len()
            && s.starts_with(STOP_AND_WAIT)
            && s.ends_with(MEMORY_NUDGE_SUFFIX))
        || (s.len() == TOOL_USE_REJECTED.len() + MEMORY_NUDGE_SUFFIX.len()
            && s.starts_with(TOOL_USE_REJECTED)
            && s.ends_with(MEMORY_NUDGE_SUFFIX))
}

/// Oracle `GI0(e)` — every block is an errored `tool_result` whose string
/// content is one of the rejection bodies.
fn is_all_rejected_tool_results(m: &JsonlMessage) -> bool {
    let Some(arr) = content(m).and_then(Value::as_array) else {
        return false;
    };
    !arr.is_empty()
        && arr.iter().all(|b| {
            b.get("type").and_then(Value::as_str) == Some("tool_result")
                && b.get("is_error").and_then(Value::as_bool) == Some(true)
                && b.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(is_rejection_body)
        })
}

/// Oracle `wEy(e)` — every block is a `tool_result` (a tool-result carrier
/// user line, i.e. the turn's own tool plumbing).
fn is_all_tool_results(m: &JsonlMessage) -> bool {
    let Some(arr) = content(m).and_then(Value::as_array) else {
        return false;
    };
    !arr.is_empty()
        && arr
            .iter()
            .all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
}

/// Oracle `g8(e)` — an interrupt/abort user entry (string content starting with
/// one of `Fzd`, or every block's text / errored tool_result content doing so).
fn is_interrupt_entry(m: &JsonlMessage) -> bool {
    if m.message_type != "user" {
        return false;
    }
    let Some(c) = content(m) else { return false };
    if let Some(text) = c.as_str() {
        return INTERRUPT_PREFIXES.iter().any(|p| text.starts_with(p));
    }
    let Some(arr) = c.as_array() else {
        return false;
    };
    !arr.is_empty()
        && arr.iter().all(|b| {
            let ty = b.get("type").and_then(Value::as_str);
            let s = match ty {
                Some("text") => b.get("text").and_then(Value::as_str),
                Some("tool_result") if b.get("is_error").and_then(Value::as_bool) == Some(true) => {
                    b.get("content").and_then(Value::as_str)
                }
                _ => None,
            };
            s.is_some_and(|s| INTERRUPT_PREFIXES.iter().any(|p| s.starts_with(p)))
        })
}

/// Oracle `KI0(e)` — the synthetic "No response requested." assistant line.
fn is_synthetic_no_response(m: &JsonlMessage) -> bool {
    if m.message.get("model").and_then(Value::as_str) != Some("<synthetic>") {
        return false;
    }
    let Some(arr) = content(m).and_then(Value::as_array) else {
        return false;
    };
    arr.len() == 1
        && arr[0].get("type").and_then(Value::as_str) == Some("text")
        && arr[0].get("text").and_then(Value::as_str) == Some("No response requested.")
}

/// Oracle `WI0(e)` — leading entries the range may carry BEFORE the declared
/// turn prompt (tail furniture of the previous turn).
///
/// ```js
/// function WI0(e){
///   if(e.type==="user")return pnu(e)&&e.isCompactSummary!==!0&&(kEy(e)||GI0(e)||e.isMeta===!0&&e.promptSource===void 0);
///   if(e.type==="assistant")return KI0(e);
///   return e.type==="system"||e.type==="progress"||e.type==="attachment"&&!qI0.has(e.attachment.type)&&CEy.has(e.attachment.type)}
/// ```
fn is_skippable_leading(m: &JsonlMessage) -> bool {
    match m.message_type.as_str() {
        "user" => {
            is_locally_sourced(m)
                && !extra_bool(m, "isCompactSummary")
                && (is_interrupt_sentinel(m)
                    || is_all_rejected_tool_results(m)
                    || (extra_bool(m, "isMeta") && !m.extra.contains_key("promptSource")))
        }
        "assistant" => is_synthetic_no_response(m),
        "system" | "progress" => true,
        "attachment" => attachment_type(m).is_some_and(|t| {
            !NON_SKIPPABLE_FURNITURE.contains(&t) && FURNITURE_ATTACHMENTS.contains(&t)
        }),
        _ => false,
    }
}

/// Oracle `qai(e,t)` — a delivered poll-event record anywhere at or after `from`.
fn contains_poll_events(range: &[JsonlMessage], from: usize) -> bool {
    range
        .iter()
        .skip(from)
        .any(|m| m.message_type == "attachment" && attachment_type(m) == Some("poll_events"))
}

/// Oracle `AEy(e,t)` — is the discarded `range` exactly the turn `turn_uuid`?
///
/// `Ok(())` is the oracle's `{ok:!0}`; `Err(reason)` its `{ok:!1,reason}`.
pub fn verify_dropped_turn(range: &[JsonlMessage], turn_uuid: &str) -> Result<(), String> {
    if !is_uuid(turn_uuid) {
        return Err(format!("declared turn id is not a UUID: {turn_uuid}"));
    }
    // Skip the previous turn's trailing furniture.
    let mut r = 0usize;
    while r < range.len() && is_skippable_leading(&range[r]) {
        r += 1;
    }
    if r == range.len() {
        return Ok(());
    }
    let first = &range[r];
    if !(first.message_type == "user" && first.uuid == turn_uuid) {
        return Err(format!(
            "range does not start with the declared turn prompt; first discarded {}",
            describe(first, r)
        ));
    }
    if extra_bool(first, "isMeta")
        || extra_bool(first, "isCompactSummary")
        || extra_bool(first, "stackedExpansion")
        || is_interrupt_entry(first)
        || is_all_tool_results(first)
    {
        return Err(format!(
            "declared turn id names a non-prompt user entry; {}",
            describe(first, r)
        ));
    }
    if !is_locally_sourced(first) {
        return Err(format!(
            "declared turn id names an externally-sourced entry; {}",
            describe(first, r)
        ));
    }
    if contains_poll_events(range, r) {
        return Err("range contains a delivered poll-event record".to_string());
    }
    for (o, entry) in range.iter().enumerate().skip(r + 1) {
        match entry.message_type.as_str() {
            "assistant" | "progress" | "system" => continue,
            "attachment" => {
                let ty = attachment_type(entry).unwrap_or_default();
                if ty == "queued_command" {
                    return Err(format!(
                        "range contains absorbed queued content; {}",
                        describe(entry, o)
                    ));
                }
                if !FURNITURE_ATTACHMENTS.contains(&ty) {
                    return Err(format!(
                        "range contains a non-furniture attachment; {}",
                        describe(entry, o)
                    ));
                }
                continue;
            }
            "user" => {
                if entry.uuid == turn_uuid {
                    continue;
                }
                if extra_bool(entry, "isCompactSummary") {
                    return Err(format!(
                        "range contains a compaction summary; {}",
                        describe(entry, o)
                    ));
                }
                if !is_locally_sourced(entry) {
                    return Err(format!(
                        "range contains an externally-sourced user entry; {}",
                        describe(entry, o)
                    ));
                }
                if extra_bool(entry, "stackedExpansion")
                    || is_interrupt_sentinel(entry)
                    || is_all_tool_results(entry)
                {
                    continue;
                }
                if extra_bool(entry, "isMeta") && entry.extra.contains_key("promptSource") {
                    return Err(format!(
                        "range contains a system-injected turn prompt; {}",
                        describe(entry, o)
                    ));
                }
                if extra_bool(entry, "isMeta") {
                    continue;
                }
                return Err(format!(
                    "range contains a user entry not attributable to the declared turn; {}",
                    describe(entry, o)
                ));
            }
            _ => {
                return Err(format!(
                    "range contains an unrecognized entry; {}",
                    describe(entry, o)
                ))
            }
        }
    }
    Ok(())
}

/// The whole `if(r.resumeSessionAt){…}` block, as a pure function over the
/// loaded transcript.
///
/// `Ok(truncated)` on success; `Err(message)` carries the byte-exact stderr line
/// the caller prints before exiting 1.
pub fn apply_truncating_resume(
    messages: Vec<JsonlMessage>,
    resume_session_at: Option<&str>,
    resume_drops_turn: Option<&str>,
) -> Result<Vec<JsonlMessage>, String> {
    let Some(at) = resume_session_at else {
        return Ok(messages);
    };
    let Some(d) = messages.iter().position(|m| m.uuid == at) else {
        return Err(format!("No message found with message.uuid of: {at}"));
    };
    if let Some(turn) = resume_drops_turn {
        if let Err(reason) = verify_dropped_turn(&messages[d + 1..], turn) {
            return Err(format!(
                "{DROP_GUARD_REFUSED_PREFIX} resuming at {at} would discard entries not \
                 attributable to turn {turn}: {reason}"
            ));
        }
    }
    let mut messages = messages;
    messages.truncate(d + 1);
    Ok(messages)
}

#[cfg(test)]
#[path = "resume_truncation_test.rs"]
mod tests;
