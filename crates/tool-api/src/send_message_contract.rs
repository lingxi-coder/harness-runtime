//! Shared Claude Code 2.1.263 SendMessage contract.
//!
//! Source: src_174624321.js (_s, Se, Ge, validateInput), with summary
//! coercion from src_160357157.js ($Gt). Both host tools use this surface.

use crate::tool_trait::CoercedInput;
use serde_json::{json, Value};
use std::sync::OnceLock;

/// Latest feature-dependent schema; ordinary agents do not require teams.
pub fn schema(cross_session: bool, teams: bool) -> &'static Value {
    static SCHEMAS: [OnceLock<Value>; 4] = [const { OnceLock::new() }; 4];
    SCHEMAS[usize::from(cross_session) * 2 + usize::from(teams)]
        .get_or_init(|| build_schema(cross_session, teams))
}

fn build_schema(cross_session: bool, teams: bool) -> Value {
    let plain = json!({"description": if cross_session {
        "Plain text message content. The recipient's human sees only the FIRST LINE as a one-line preview until they expand it, so make the first line a clear, self-contained sentence saying what this is about — not a greeting, preamble, or bare @-mention."
    } else { "Plain text message content" }, "type":"string"});
    let request_id = json!({"type":"string", "minLength":1,
        "allOf":[{"pattern":"^[^\\n\\r]*$"},{"pattern":"^[\\s\\S]{0,300}$"}]});
    let structured = json!({"anyOf":[
        {"type":"object", "properties":{"type":{"type":"string","const":"shutdown_request"},"reason":{"type":"string"}},"required":["type"],"additionalProperties":false},
        {"type":"object", "properties":{"type":{"type":"string","const":"shutdown_response"},"request_id":request_id,"approve":{"type":"boolean"},"reason":{"type":"string"}},"required":["type","request_id","approve"],"additionalProperties":false},
        {"type":"object", "properties":{"type":{"type":"string","const":"plan_approval_response"},"request_id":request_id,"approve":{"type":"boolean"},"feedback":{"type":"string"}},"required":["type","request_id","approve"],"additionalProperties":false}
    ]});
    let mut message = if teams {
        json!({"anyOf":[plain,structured]})
    } else {
        plain
    };
    if cross_session {
        let mut defaulted = serde_json::Map::new();
        defaulted.insert("default".into(), json!(""));
        if let Value::Object(properties) = message {
            defaulted.extend(properties);
        }
        message = Value::Object(defaulted);
    }
    let mut result = json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "properties":{
        "to":{"description":if cross_session {
            "Recipient: a name from ListAgents (append its \" [ref]\" only when a listing or an error shows one), a teammate name, \"main\", or a background agent's agentId"
        } else {"Recipient: teammate name"}, "type":"string", "allOf":[{"pattern":"^[^\\n\\r]*$"},{"pattern":"^[\\s\\S]{0,300}$"}]},
        "summary":{"description":if cross_session {
            "A 5-10 word label for your own transcript row (not transmitted — the recipient previews the first line of `message`). Truncated to 200 characters rather than rejected."
        } else {"A 5-10 word summary shown as a one-line preview in the UI. Defaults to the first line of a plain-text message; longer summaries are truncated to 200 characters rather than rejected."}, "type":"string", "maxLength":200},
        "message":message
    },"required":["to","message"],"additionalProperties":false});
    if cross_session {
        result["properties"]["notify_when_idle"] = json!({"description":"Ask a session ON THIS MACHINE to send you ONE notice when it next goes idle (finishes its turn with nothing queued) or exits — opt-in, one-shot, no polling. With a message: deliver it now AND subscribe. Without a message (omit it): a pure subscription that costs the other session nothing.","type":"boolean"});
    }
    result
}

/// Derive a missing summary before schema validation, or truncate a long one.
pub fn coerce(input: &Value) -> Option<CoercedInput> {
    let mut summary = input
        .get("summary")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut shape_class = None;
    if summary.as_deref().is_none_or(|s| s.trim().is_empty()) {
        if let Some(message) = input.get("message").and_then(Value::as_str) {
            let first = message.trim().split('\n').next().unwrap_or("").trim();
            if !first.is_empty() {
                summary = Some(first.to_owned());
                shape_class = Some("derive_summary");
            }
        }
    }
    if let Some(value) = summary.as_mut() {
        if value.encode_utf16().count() > 200 {
            *value = value.chars().take(199).collect::<String>() + "…";
            shape_class.get_or_insert("truncate_summary");
        }
    }
    shape_class.map(|shape_class| {
        let mut input = input.clone();
        input["summary"] = json!(summary);
        CoercedInput {
            input,
            shape_class: shape_class.into(),
        }
    })
}

/// Plain-text protocol frames cannot bypass the structured-message handlers.
pub fn plain_message_error(message: &str) -> Option<&'static str> {
    let parsed: Value = serde_json::from_str(message).ok()?;
    let kind = parsed.get("type")?.as_str()?;
    if matches!(
        kind,
        "permission_request"
            | "permission_response"
            | "sandbox_permission_request"
            | "sandbox_permission_response"
            | "shutdown_request"
            | "shutdown_response"
            | "shutdown_approved"
            | "team_permission_update"
            | "mode_set_request"
            | "plan_approval_request"
            | "plan_approval_response"
    ) {
        return Some("message text must not be a teammate protocol frame (permission/mode/plan/shutdown JSON) — to respond to a plan or shutdown request, use the structured object form ({\"message\": {\"type\": ...}}); otherwise send plain text");
    }
    if matches!(
        kind,
        "idle_notification"
            | "teammate_terminated"
            | "task_assignment"
            | "task_completed"
            | "shutdown_rejected"
    ) {
        return Some("message text must not be a teammate lifecycle/task frame (idle/terminated/task/shutdown JSON) — send plain text instead");
    }
    None
}

/// Exact Ge(zr()) output for the corresponding feature gates.
pub fn prompt(cross_session: bool, teams: bool) -> &'static str {
    match (cross_session, teams) {
        (false, false) => {
            r###"# SendMessage

Send a message to another agent.

```json
{"to": "researcher", "summary": "assign task 1", "message": "start on task #1"}
```

| `to` | |
|---|---|
| `"researcher"` | Teammate by name |
| `"main"` | The main conversation (background subagents only) |

Your plain text output is NOT visible to other agents — to communicate, you MUST call this tool. Messages from teammates are delivered automatically; you don't check an inbox. Refer to agents by name — names keep working after an agent completes (a send resumes it from its transcript). Use the raw `agentId` (format `a...-...`) from its spawn result only when the agent has no name, or when a newer agent took the name (latest wins). When relaying, don't quote the original — it's already rendered to the user."###
        }
        (false, true) => {
            r###"# SendMessage

Send a message to another agent.

```json
{"to": "researcher", "summary": "assign task 1", "message": "start on task #1"}
```

| `to` | |
|---|---|
| `"researcher"` | Teammate by name |
| `"main"` | The main conversation (background subagents only) |

Your plain text output is NOT visible to other agents — to communicate, you MUST call this tool. Messages from teammates are delivered automatically; you don't check an inbox. Refer to agents by name — names keep working after an agent completes (a send resumes it from its transcript). Use the raw `agentId` (format `a...-...`) from its spawn result only when the agent has no name, or when a newer agent took the name (latest wins). When relaying, don't quote the original — it's already rendered to the user.

## Protocol responses (legacy)

If you receive a JSON message with `type: "shutdown_request"` or `type: "plan_approval_request"`, respond with the matching `_response` type — echo the `request_id`, set `approve` true/false:

```json
{"to": "team-lead", "message": {"type": "shutdown_response", "request_id": "...", "approve": true}}
{"to": "researcher", "message": {"type": "plan_approval_response", "request_id": "...", "approve": false, "feedback": "add error handling"}}
```

Approving shutdown terminates your process. Rejecting plan sends the teammate back to revise. Don't originate `shutdown_request` unless asked. Don't send structured JSON status messages — report progress through your task tools if you have them, otherwise in plain prose."###
        }
        (true, false) => {
            r###"# SendMessage

Send a message to another agent.

```json
{"to": "researcher", "summary": "assign task 1", "message": "start on task #1"}
```

| `to` | |
|---|---|
| `"researcher"` | Teammate by name |
| `"main"` | The main conversation (background subagents only) |
| `"worker"` | Any agent from `ListAgents` — subagent, another local Claude session |
| `"worker [3fa9c1]"` | Same, plus its `[ref]` — only when a listing or an error shows one |

Your plain text output is NOT visible to other agents — to communicate, you MUST call this tool. Messages from teammates are delivered automatically; you don't check an inbox. Refer to agents by name — names keep working after an agent completes (a send resumes it from its transcript). Use the raw `agentId` (format `a...-...`) from its spawn result only when the agent has no name, or when a newer agent took the name (latest wins). When relaying, don't quote the original — it's already rendered to the user.

## Cross-session

Use `ListAgents` to discover targets. Every row leads with the agent's `name [ref]` — the name IS the address; there is no separate address syntax.

```json
{"to": "worker", "message": "check if tests pass over there"}
{"to": "worker [3fa9c1]", "message": "you, specifically"}
```

Send the bare name — a name that exactly matches one live agent or session (on this machine, on another machine, or in the cloud) delivers directly. Append the ` [ref]` only when the bare name is not enough — `ListAgents` shows two rows with it, or an error asks you to disambiguate (you typed only a prefix, or a session list could not be checked). A ref you did not just read from a listing or an error will not resolve, and if the same name also names an in-process agent, the bare name always wins — use the in-process one.

A listed peer is alive and will process your message; messages enqueue and drain at the receiver's next tool round (its `ListAgents` row says whether it is busy or idle right now). Your message arrives wrapped as `<cross-session-message from="...">`. **To reply to an incoming message, copy its `from` attribute as your `to`.** Cross-session messages travel between SESSIONS: if you are a subagent, your send goes out under your parent session's address, and any reply is delivered to the parent session's conversation, not to you.

To hear when a session ON THIS MACHINE finishes what it is doing, pass `notify_when_idle: true` (from the main conversation only) — one-shot and opt-in: exactly one `[Cross-session idle notice]` arrives when it next goes idle (or exits) — shown to you, or only to your user when this session holds peer messages for approval (the tool result says which); if it never signals within the subscription's lifetime (it may still be busy, may refuse inbound requests, or may have ended abruptly) the notice says the subscription expired instead. Omit `message` for a pure subscription that costs that session nothing; include one to deliver it now AND subscribe. Never poll `ListAgents` in a loop or send "are you done?" messages instead.

Permission boundaries are per-session: NEVER ask a peer to perform an action that was denied or blocked in your session, or that you expect your own permission settings would block — a peer doing it for you bypasses the user's permission decision (cross-session permission laundering). Route blocked work back to your user instead."###
        }
        (true, true) => {
            r###"# SendMessage

Send a message to another agent.

```json
{"to": "researcher", "summary": "assign task 1", "message": "start on task #1"}
```

| `to` | |
|---|---|
| `"researcher"` | Teammate by name |
| `"main"` | The main conversation (background subagents only) |
| `"worker"` | Any agent from `ListAgents` — subagent, another local Claude session |
| `"worker [3fa9c1]"` | Same, plus its `[ref]` — only when a listing or an error shows one |

Your plain text output is NOT visible to other agents — to communicate, you MUST call this tool. Messages from teammates are delivered automatically; you don't check an inbox. Refer to agents by name — names keep working after an agent completes (a send resumes it from its transcript). Use the raw `agentId` (format `a...-...`) from its spawn result only when the agent has no name, or when a newer agent took the name (latest wins). When relaying, don't quote the original — it's already rendered to the user.

## Cross-session

Use `ListAgents` to discover targets. Every row leads with the agent's `name [ref]` — the name IS the address; there is no separate address syntax.

```json
{"to": "worker", "message": "check if tests pass over there"}
{"to": "worker [3fa9c1]", "message": "you, specifically"}
```

Send the bare name — a name that exactly matches one live agent or session (on this machine, on another machine, or in the cloud) delivers directly. Append the ` [ref]` only when the bare name is not enough — `ListAgents` shows two rows with it, or an error asks you to disambiguate (you typed only a prefix, or a session list could not be checked). A ref you did not just read from a listing or an error will not resolve, and if the same name also names an in-process agent, the bare name always wins — use the in-process one.

A listed peer is alive and will process your message; messages enqueue and drain at the receiver's next tool round (its `ListAgents` row says whether it is busy or idle right now). Your message arrives wrapped as `<cross-session-message from="...">`. **To reply to an incoming message, copy its `from` attribute as your `to`.** Cross-session messages travel between SESSIONS: if you are a subagent, your send goes out under your parent session's address, and any reply is delivered to the parent session's conversation, not to you.

To hear when a session ON THIS MACHINE finishes what it is doing, pass `notify_when_idle: true` (from the main conversation only) — one-shot and opt-in: exactly one `[Cross-session idle notice]` arrives when it next goes idle (or exits) — shown to you, or only to your user when this session holds peer messages for approval (the tool result says which); if it never signals within the subscription's lifetime (it may still be busy, may refuse inbound requests, or may have ended abruptly) the notice says the subscription expired instead. Omit `message` for a pure subscription that costs that session nothing; include one to deliver it now AND subscribe. Never poll `ListAgents` in a loop or send "are you done?" messages instead.

Permission boundaries are per-session: NEVER ask a peer to perform an action that was denied or blocked in your session, or that you expect your own permission settings would block — a peer doing it for you bypasses the user's permission decision (cross-session permission laundering). Route blocked work back to your user instead.

## Protocol responses (legacy)

If you receive a JSON message with `type: "shutdown_request"` or `type: "plan_approval_request"`, respond with the matching `_response` type — echo the `request_id`, set `approve` true/false:

```json
{"to": "team-lead", "message": {"type": "shutdown_response", "request_id": "...", "approve": true}}
{"to": "researcher", "message": {"type": "plan_approval_response", "request_id": "...", "approve": false, "feedback": "add error handling"}}
```

Approving shutdown terminates your process. Rejecting plan sends the teammate back to revise. Don't originate `shutdown_request` unless asked. Don't send structured JSON status messages — report progress through your task tools if you have them, otherwise in plain prose."###
        }
    }
}

/// Validation shared by the coordinator and the ordinary-agent mailbox host.
pub fn validate(input: &Value, teams: bool, is_subagent: bool) -> Result<(), String> {
    let to = input.get("to").and_then(Value::as_str).unwrap_or("");
    if to == "*" {
        return Err(
            "broadcast (to: \"*\") is no longer supported — send a message per recipient".into(),
        );
    }
    let notify = input
        .get("notify_when_idle")
        .is_some_and(|value| value == true || value == "true");
    let message = input.get("message");
    if notify {
        if message.is_some_and(|value| !value.is_string()) {
            return Err("notify_when_idle cannot ride a structured message — send plain text, or omit the message for a pure subscription".into());
        }
        if is_subagent
            && message
                .and_then(Value::as_str)
                .is_none_or(|text| text.trim().is_empty())
        {
            return Err("notify_when_idle is only available from the main conversation of this session (not from a subagent or teammate).".into());
        }
    }
    if to.trim().is_empty() {
        return Err("to must not be empty".into());
    }
    if to.contains(['\n', '\r']) {
        return Err("must be a single-line recipient name or address".into());
    }
    if to.chars().count() > 300 {
        return Err("recipient longer than any listed name or address (max 300 characters)".into());
    }
    if to.contains('@') {
        return Err("to must be a bare teammate name — there is only one team per session".into());
    }
    let Some(message) = message else {
        return if notify {
            Ok(())
        } else {
            Err("message is required unless notify_when_idle is true".into())
        };
    };
    if let Some(message) = message.as_str() {
        if message.trim().is_empty() && !notify {
            return Err("message must not be empty".into());
        }
        return plain_message_error(message).map_or(Ok(()), |error| Err(error.into()));
    }
    if !teams {
        return Err(
            "Structured team-protocol messages are only available with agent teams enabled.".into(),
        );
    }
    if to.starts_with("session:") || to.starts_with("uds:") || to.starts_with("bridge:") {
        return Err("structured messages cannot be sent cross-session — only plain text".into());
    }
    if message.get("type").and_then(Value::as_str) == Some("shutdown_response") {
        if to != "team-lead" {
            return Err("shutdown_response must be sent to \"team-lead\"".into());
        }
        let approved = message
            .get("approve")
            .is_some_and(|value| value == true || value == "true");
        if approved && message.get("reason").is_some() {
            return Err("reason is only delivered on rejections (approve: false) — approvals are sent as a silent confirmation with no reason text; omit reason or reject instead".into());
        }
        if !approved
            && message
                .get("reason")
                .and_then(Value::as_str)
                .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err("reason is required when rejecting a shutdown request".into());
        }
    }
    Ok(())
}

/// ISO timestamp matching Date.toISOString, with an explicit clock for tests.
pub fn protocol_timestamp(timestamp: std::time::SystemTime) -> String {
    let duration = timestamp
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();
    let millis = duration.subsec_millis();
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60
    )
}

/// Ordered nwn envelope; absent pane fields serialize as absent, never null.
pub fn shutdown_approved(
    request_id: &str,
    from: &str,
    timestamp: &str,
    pane_id: Option<&str>,
    backend_type: Option<&str>,
) -> Value {
    let mut frame = json!({"type":"shutdown_approved","requestId":request_id,"from":from,"timestamp":timestamp});
    if let Some(pane_id) = pane_id {
        frame["paneId"] = json!(pane_id);
    }
    if let Some(backend_type) = backend_type {
        frame["backendType"] = json!(backend_type);
    }
    frame
}

/// Ordered rwn envelope from the same current oracle module.
pub fn shutdown_rejected(request_id: &str, from: &str, reason: &str, timestamp: &str) -> Value {
    json!({"type":"shutdown_rejected","requestId":request_id,"from":from,"reason":reason,"timestamp":timestamp})
}

/// Ordered twn envelope. Undefined reason is omitted.
pub fn shutdown_request(
    request_id: &str,
    from: &str,
    reason: Option<&str>,
    timestamp: &str,
) -> Value {
    let mut frame = json!({"type":"shutdown_request","requestId":request_id,"from":from});
    if let Some(reason) = reason {
        frame["reason"] = json!(reason);
    }
    frame["timestamp"] = json!(timestamp);
    frame
}

/// Current plan response envelope; approved responses carry the leader's live mode.
pub fn plan_response(
    request_id: &str,
    approved: bool,
    feedback: Option<&str>,
    timestamp: &str,
    permission_mode: Option<&str>,
) -> Value {
    let mut frame =
        json!({"type":"plan_approval_response","requestId":request_id,"approved":approved});
    if let Some(feedback) = feedback {
        frame["feedback"] = json!(feedback);
    }
    frame["timestamp"] = json!(timestamp);
    if approved {
        if let Some(mode) = permission_mode {
            frame["permissionMode"] = json!(mode);
        }
    }
    frame
}

/// Oracle Gs routing field order, omitting unavailable colors like undefined.
pub fn routing(
    sender: &str,
    sender_color: Option<&str>,
    target: &str,
    target_color: Option<&str>,
    summary: Option<&str>,
    content: Option<&str>,
) -> Value {
    let mut frame = json!({"sender":sender});
    if let Some(color) = sender_color {
        frame["senderColor"] = json!(color);
    }
    frame["target"] = json!(target);
    if let Some(color) = target_color {
        frame["targetColor"] = json!(color);
    }
    if let Some(summary) = summary {
        frame["summary"] = json!(summary);
    }
    if let Some(content) = content {
        frame["content"] = json!(content);
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_and_prompts_match_real_binary_wire_captures_byte_for_byte() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../test-harness/src/parity/fixtures/send_message_2_1_263_wire.json"
        ))
        .unwrap();
        assert_eq!(fixture["version"], "2.1.263");
        for case in fixture["cases"].as_array().unwrap() {
            let cross = case["cross_session"].as_bool().unwrap();
            let teams = case["teams"].as_bool().unwrap();
            assert_eq!(
                serde_json::to_string(schema(cross, teams)).unwrap(),
                case["input_schema_json"].as_str().unwrap(),
                "schema flags cross={cross} teams={teams}"
            );
            assert_eq!(
                prompt(cross, teams),
                case["prompt"].as_str().unwrap(),
                "prompt flags cross={cross} teams={teams}"
            );
        }
    }

    #[test]
    fn shutdown_envelopes_match_oracle_with_fixed_clock() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../test-harness/src/parity/fixtures/send_message_2_1_263_wire.json"
        ))
        .unwrap();
        let timestamp = "2026-09-08T12:34:56.789Z";
        for case in fixture["shutdown_envelopes"].as_array().unwrap() {
            let input = &case["input"];
            let request_id = input["requestId"].as_str().unwrap();
            let from = input["from"].as_str().unwrap();
            let frame = match case["kind"].as_str().unwrap() {
                "approved" => shutdown_approved(
                    request_id,
                    from,
                    timestamp,
                    input["paneId"].as_str(),
                    input["backendType"].as_str(),
                ),
                "rejected" => shutdown_rejected(
                    request_id,
                    from,
                    input["reason"].as_str().unwrap(),
                    timestamp,
                ),
                "request" => {
                    shutdown_request(request_id, from, input["reason"].as_str(), timestamp)
                }
                _ => unreachable!(),
            };
            assert_eq!(frame.to_string(), case["json"].as_str().unwrap());
        }
        assert_eq!(
            protocol_timestamp(std::time::UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            protocol_timestamp(
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_788_870_896_789)
            ),
            timestamp
        );
    }

    #[test]
    fn plan_response_fields_and_order_match_current_oracle() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../test-harness/src/parity/fixtures/send_message_2_1_263_wire.json"
        ))
        .unwrap();
        for case in fixture["plan_responses"].as_array().unwrap() {
            let frame = plan_response(
                "plan-1@scout",
                case["approved"].as_bool().unwrap(),
                case["feedback"].as_str(),
                "2026-09-08T12:34:56.789Z",
                case["mode"].as_str(),
            );
            assert_eq!(frame.to_string(), case["json"].as_str().unwrap());
            assert!(frame.get("from").is_none());
        }
    }

    #[test]
    fn routing_colors_and_field_order_match_oracle() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../test-harness/src/parity/fixtures/send_message_2_1_263_wire.json"
        ))
        .unwrap();
        for case in fixture["routing_cases"].as_array().unwrap() {
            let frame = routing(
                case["sender"].as_str().unwrap(),
                case["senderColor"].as_str(),
                case["target"].as_str().unwrap(),
                case["targetColor"].as_str(),
                case["summary"].as_str(),
                case["content"].as_str(),
            );
            assert_eq!(frame.to_string(), case["json"].as_str().unwrap());
        }
    }

    #[test]
    fn summaries_derive_first_trimmed_line_and_truncate() {
        let coerced =
            coerce(&json!({"to":"worker","message":"  First line\nSecond line"})).unwrap();
        assert_eq!(coerced.input["summary"], "First line");
        assert_eq!(coerced.shape_class, "derive_summary");
        let coerced =
            coerce(&json!({"to":"worker","message":"body","summary":"x".repeat(201)})).unwrap();
        assert_eq!(coerced.input["summary"], format!("{}…", "x".repeat(199)));
        assert_eq!(coerced.shape_class, "truncate_summary");
    }

    #[test]
    fn schemas_gate_only_protocol_messages_and_cross_session_options() {
        for cross in [false, true] {
            for teams in [false, true] {
                let value = schema(cross, teams);
                assert_eq!(value["properties"].get("notify_when_idle").is_some(), cross);
                assert_eq!(value["properties"]["message"].get("anyOf").is_some(), teams);
                assert_eq!(value["required"], json!(["to", "message"]));
                assert_eq!(
                    prompt(cross, teams).contains("## Protocol responses"),
                    teams
                );
                assert_eq!(prompt(cross, teams).contains("## Cross-session"), cross);
            }
        }
    }

    #[test]
    fn lifecycle_and_permission_frames_are_rejected_but_prose_is_allowed() {
        for kind in [
            "permission_request",
            "shutdown_approved",
            "mode_set_request",
            "idle_notification",
            "task_assignment",
        ] {
            assert!(plain_message_error(&json!({"type":kind}).to_string()).is_some());
        }
        assert_eq!(plain_message_error("The task is complete."), None);
        assert_eq!(plain_message_error(r#"{"type":"application-event"}"#), None);
    }
}
