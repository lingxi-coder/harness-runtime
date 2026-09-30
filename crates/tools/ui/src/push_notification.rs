//! `PushNotificationTool` — the `/loop`-adjacent `PushNotification` tool.
//!
//! 1:1 Rust port of claude-code's `PushNotification` tool object (`Wzp=Xs({…})`,
//! name const `Z8 = "PushNotification"`). It posts a desktop notification in the
//! user's terminal and — when Remote Control is connected — also pushes to their
//! phone, pulling their attention to the session for something worth coming back
//! for (a long task finished, a build is ready, a decision is needed).
//!
//! **Gated off by default.** `isEnabled` reads `tengu_kairos_push_notifications`
//! (binary `q7(...,false,jzp)`), which has no live GrowthBook backend in the port
//! → the tool is registered but DISABLED, so it is invisible to the model and
//! byte-identical to the shipped binary's default config.
//!
//! **Capability seams.** The binary's transport/presence probes map to port seams
//! that default to "absent" — exactly the binary's behavior on a terminal-only
//! host with no Remote Control and no focus/idle tracking:
//!   - `sa()` (remote workspace) → false; `is_remote` honors only `LINGXI_REMOTE`.
//!   - `mH()` (REPL-bridge mobile-push transport) → false → no mobile transport.
//!   - `mc("agentPushNotifEnabled",false)` setting → false (no setting backend).
//!   - `xur()` (user-present: focus `H1e()` or last-keystroke `N0()` within
//!     `hZt`=60s) → false (no interaction tracking → not present → don't suppress);
//!     `idleSec`/`hasFocus` are omitted.
//!   - the local egress `o?.({type:"os_notification",…})` is an optional sink that
//!     is a no-op here (no desktop-notification backend wired); `localSent` still
//!     reflects the interactive session, exactly like the binary.
//! With these defaults a successful (flag-on) call takes the `no_transport`
//! branch — terminal notification "sent", mobile push not — matching the binary on
//! a host without Remote Control.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Binary `Z8` — the tool name.
pub const PUSH_NOTIFICATION_TOOL_NAME: &str = "PushNotification";

/// Binary `tengu_kairos_push_notifications` — the `isEnabled` flag.
const PUSH_NOTIFICATION_FLAG: &str = "tengu_kairos_push_notifications";

/// Binary `hZt` — the user-present idle threshold (ms): a keystroke within this
/// window counts as "present". 60s.
const IDLE_THRESHOLD_MS: u64 = 60_000;

/// Binary `zVi` — the tool description.
const DESCRIPTION: &str = "Send a host notification to the user and, when Remote Control is connected, also send a remote push";

/// Binary `KVi` — the tool prompt (`YVi()` returns `KVi` outside a remote-trigger
/// routine; the `q4d` routine addendum is appended only when
/// `CLAUDE_CODE_ENTRYPOINT=="remote_trigger"`). Byte-exact incl. em-dashes (U+2014).
const PROMPT: &str = "This tool sends a host notification. If Remote Control is connected, it also sends a remote push. Either way, it pulls their attention from whatever they're doing — a meeting, another task, dinner — to this session. That's the cost. The benefit is they learn something now that they'd want to know now: a long task finished while they were away, a build is ready, you've hit something that needs their decision before you can continue.
Because a notification they didn't need is annoying in a way that accumulates, err toward not sending one. Don't notify for routine progress, or to announce you've answered something they asked seconds ago and are clearly still watching, or when a quick task completes. Notify when there's a real chance they've walked away and there's something worth coming back for — or when they've explicitly asked you to notify them.
Keep the message under 200 characters, one line, no markdown. Lead with what they'd act on — \"build failed: 2 auth tests\" tells them more than \"task done\" and more than a status dump.
If the result says the push wasn't sent, that's expected — no action needed.";

/// Binary `q4d` — the scheduled-routine addendum (`_in()` = entrypoint
/// `remote_trigger`). `$4d` = `<routine_summary>`.
const ROUTINE_ADDENDUM: &str = "
This is a scheduled routine — the notification is how the run reaches its owner. Wrap the message in <routine_summary> tags: the first sentence becomes the phone banner, the full text becomes the email body.";

/// Binary `$zp` — the input schema: `strictObject({message, status:literal("proactive")})`.
static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "message": {
                "type": "string",
                "minLength": 1,
                "description": "The notification body. Keep it under 200 characters; mobile OSes truncate."
            },
            "status": { "type": "string", "const": "proactive" }
        },
        "required": ["message", "status"]
    })
});

/// Binary `qzp` — the output schema.
static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "message": { "type": "string" },
            "pushSent": { "type": "boolean" },
            "localSent": { "type": "boolean" },
            "disabledReason": { "type": "string", "enum": ["config_off", "user_present", "no_transport"] },
            "idleSec": { "type": "number" },
            "hasFocus": { "type": "boolean" },
            "sentAt": { "type": "string" }
        },
        "required": ["message"]
    })
});

/// `PushNotification` — post a terminal/mobile notification (binary `Wzp`).
pub struct PushNotificationTool {
    _ctx: BuiltinToolContext,
}

impl PushNotificationTool {
    /// Construct the tool over the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { _ctx: ctx }
    }
}

// ── Capability seams (binary `sa`/`mH`/`xur`/`mc`/`_in`) ──────────────────────

/// `rt(process.env.LINGXI_REMOTE) || sa()` — is this a remote session? The
/// port has no `Nt.caps.workspace` remote-workspace tracking (`sa()`=false), so
/// only the env override applies.
fn is_remote() -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var("LINGXI_REMOTE").ok().as_deref())
}

/// `mH()` (`Nt.replBridgeActive`) — is a mobile-push transport (Remote Control /
/// REPL bridge) connected? No REPL-bridge subsystem in the port → false.
fn mobile_push_available() -> bool {
    false
}

/// `mc("agentPushNotifEnabled",false).value` — the merged per-session
/// mobile-push opt-in.
fn agent_push_notif_enabled() -> bool {
    lingxi_core::host::session_flags::agent_push_notif_enabled()
}

/// `xur()` — is the user present (focus `H1e()` known, else last interaction `N0()`
/// within `hZt`)? The port has no focus / interaction-time tracking → not present.
fn user_present() -> bool {
    false
}

/// `_in()` — running as a scheduled routine (`CLAUDE_CODE_ENTRYPOINT=="remote_trigger"`).
fn is_routine() -> bool {
    std::env::var("CLAUDE_CODE_ENTRYPOINT").as_deref() == Ok("remote_trigger")
}

/// Binary `mapToolResultToToolResultBlockParam` — the model-facing render keyed on
/// the result fields. Byte-exact.
fn render(
    disabled_reason: Option<&str>,
    local_sent: bool,
    has_focus: Option<bool>,
    idle_sec: Option<u64>,
) -> String {
    match disabled_reason {
        Some("config_off") => "Push not sent — mobile push is disabled in /config.".to_string(),
        Some("user_present") => {
            if has_focus == Some(true) {
                "Not sent — the host app has focus. Local + remote notification suppressed."
                    .to_string()
            } else {
                let threshold = IDLE_THRESHOLD_MS / 1000;
                let idle = match idle_sec {
                    Some(s) => format!("{s}s"),
                    None => format!("<{threshold}s"),
                };
                format!(
                    "Not sent — user active (last interaction {idle} ago, threshold {threshold}s). Local + remote notification suppressed."
                )
            }
        }
        Some("no_transport") => {
            if local_sent {
                "Host notification sent. Remote push not sent (Remote Control inactive)."
                    .to_string()
            } else {
                "Remote push not sent (Remote Control inactive).".to_string()
            }
        }
        _ => {
            if local_sent {
                "Host notification sent. Remote push requested.".to_string()
            } else {
                "Remote push requested.".to_string()
            }
        }
    }
}

#[async_trait]
impl Tool for PushNotificationTool {
    fn name(&self) -> &str {
        PUSH_NOTIFICATION_TOOL_NAME
    }

    fn search_hint(&self) -> Option<&str> {
        Some("send a host notification to the user and optionally a remote push")
    }

    fn user_facing_name(&self) -> Option<&str> {
        Some(PUSH_NOTIFICATION_TOOL_NAME)
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // PARITY: binary `q7("tengu_kairos_push_notifications",false,jzp)`. No live
        // GrowthBook → default false → registered-but-disabled (invisible to the
        // model), byte-identical to the shipped binary.
        telemetry::flag_bool(PUSH_NOTIFICATION_FLAG, false)
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        1000
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        // PARITY: binary `checkPermissions(e){return{behavior:"allow",updatedInput:e}}`.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "PushNotification: always allowed".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        DESCRIPTION.to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        // PARITY: `YVi()` = `_in() ? KVi + q4d : KVi`.
        if is_routine() {
            format!("{PROMPT}{ROUTINE_ADDENDUM}")
        } else {
            PROMPT.to_string()
        }
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let message = input
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let sent_at = now_iso8601();

        let is_remote = is_remote();
        // `a = is_remote || mH()` — is any transport available?
        let has_transport = is_remote || mobile_push_available();

        // PARITY: config_off — transport exists, not remote, mobile-push opt-in off.
        if has_transport && !is_remote && !agent_push_notif_enabled() {
            telemetry::emit_push_notification_send(
                message.len(),
                false,
                false,
                is_remote,
                "config_off",
            );
            return Ok(result(
                &message,
                false,
                false,
                Some("config_off"),
                None,
                None,
                &sent_at,
            ));
        }

        // PARITY: user_present — not remote and the user is actively at the terminal.
        if !is_remote && user_present() {
            telemetry::emit_push_notification_send(
                message.len(),
                false,
                false,
                is_remote,
                "user_present",
            );
            return Ok(result(
                &message,
                false,
                false,
                Some("user_present"),
                // The port has no idle/focus tracking; these are omitted (the binary
                // would carry `idleSec`/`hasFocus` from `N0()`/`H1e()`).
                None,
                None,
                &sent_at,
            ));
        }

        // PARITY: local egress `o?.({type:"os_notification",…})`. Optional sink —
        // a no-op here (no desktop-notification backend wired). `localSent` reflects
        // the interactive session, exactly like the binary (`c=!isNonInteractiveSession`).
        let local_sent = !ctx.options.is_non_interactive_session;

        // PARITY: no_transport — no remote + no mobile push.
        if !has_transport {
            telemetry::emit_push_notification_send(
                message.len(),
                false,
                local_sent,
                is_remote,
                "no_transport",
            );
            return Ok(result(
                &message,
                false,
                local_sent,
                Some("no_transport"),
                None,
                None,
                &sent_at,
            ));
        }

        // PARITY: success.
        telemetry::emit_push_notification_send(message.len(), true, local_sent, is_remote, "");
        Ok(result(
            &message, true, local_sent, None, None, None, &sent_at,
        ))
    }
}

/// Build the `ToolCallResult` (binary `{data:{…}}`) + the model-facing render.
#[allow(clippy::too_many_arguments)]
fn result(
    message: &str,
    push_sent: bool,
    local_sent: bool,
    disabled_reason: Option<&str>,
    idle_sec: Option<u64>,
    has_focus: Option<bool>,
    sent_at: &str,
) -> ToolCallResult {
    let mut data = serde_json::Map::new();
    data.insert("message".into(), json!(message));
    data.insert("pushSent".into(), json!(push_sent));
    data.insert("localSent".into(), json!(local_sent));
    if let Some(r) = disabled_reason {
        data.insert("disabledReason".into(), json!(r));
    }
    if let Some(s) = idle_sec {
        data.insert("idleSec".into(), json!(s));
    }
    if let Some(f) = has_focus {
        data.insert("hasFocus".into(), json!(f));
    }
    data.insert("sentAt".into(), json!(sent_at));

    ToolCallResult {
        data: Value::Object(data),
        // The faithful model-facing render (binary mapToolResultToToolResultBlockParam).
        model_content: Some(render(disabled_reason, local_sent, has_focus, idle_sec)),
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

/// `new Date().toISOString()` — UTC ISO-8601 with millisecond precision + `Z`.
fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    // Decompose to civil date/time (UTC) without pulling in chrono here (the crate
    // already has no chrono dep). Days since epoch → y/m/d via the standard
    // civil-from-days algorithm.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Civil date from days-since-1970 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("LINGXI_REMOTE");
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        telemetry::test_clear_flag(PUSH_NOTIFICATION_FLAG);
        lingxi_core::host::session_flags::set_agent_push_notif_enabled(false);
        g
    }

    #[test]
    fn name_and_metadata_byte_exact() {
        let _g = guard();
        let t = PushNotificationTool::new(shell_test_ctx(dummy_out()));
        assert_eq!(t.name(), "PushNotification");
        assert_eq!(
            t.search_hint(),
            Some("send a host notification to the user and optionally a remote push")
        );
        assert!(t.should_defer());
        assert_eq!(t.max_result_size_chars(), 1000);
        assert!(t.is_read_only(&json!({})));
        assert!(t.is_concurrency_safe(&json!({})));
    }

    #[test]
    fn disabled_by_default_enabled_by_flag() {
        let _g = guard();
        let t = PushNotificationTool::new(shell_test_ctx(dummy_out()));
        let ctx = ToolStaticContext::default();
        // Default: flag off → disabled (invisible to the model).
        assert!(!t.is_enabled(&ctx));
        telemetry::test_set_flag(PUSH_NOTIFICATION_FLAG, true);
        assert!(t.is_enabled(&ctx));
        telemetry::test_clear_flag(PUSH_NOTIFICATION_FLAG);
    }

    #[tokio::test]
    async fn default_host_takes_no_transport_branch() {
        let _g = guard();
        let t = PushNotificationTool::new(shell_test_ctx(dummy_out()));
        let out = t
            .call(
                json!({"message": "build failed: 2 auth tests", "status": "proactive"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        // No remote + no mobile transport → no_transport; interactive → localSent.
        assert_eq!(out.data["pushSent"], json!(false));
        assert_eq!(out.data["localSent"], json!(true));
        assert_eq!(out.data["disabledReason"], json!("no_transport"));
        assert_eq!(out.data["message"], json!("build failed: 2 auth tests"));
        assert!(out.data["sentAt"].as_str().unwrap().ends_with('Z'));
        assert_eq!(
            out.model_content.as_deref(),
            Some("Host notification sent. Remote push not sent (Remote Control inactive).")
        );
    }

    #[tokio::test]
    async fn remote_session_succeeds() {
        let _g = guard();
        std::env::set_var("LINGXI_REMOTE", "1");
        let t = PushNotificationTool::new(shell_test_ctx(dummy_out()));
        let out = t
            .call(
                json!({"message": "done", "status": "proactive"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        // Remote → has transport, not config_off (is_remote), not user_present →
        // success.
        assert_eq!(out.data["pushSent"], json!(true));
        assert!(out.data.get("disabledReason").is_none());
        std::env::remove_var("LINGXI_REMOTE");
    }

    #[test]
    fn render_branches_byte_exact() {
        assert_eq!(
            render(Some("config_off"), false, None, None),
            "Push not sent — mobile push is disabled in /config."
        );
        assert_eq!(
            render(Some("user_present"), false, Some(true), None),
            "Not sent — the host app has focus. Local + remote notification suppressed."
        );
        assert_eq!(
            render(Some("user_present"), false, None, Some(12)),
            "Not sent — user active (last interaction 12s ago, threshold 60s). Local + remote notification suppressed."
        );
        assert_eq!(
            render(Some("user_present"), false, None, None),
            "Not sent — user active (last interaction <60s ago, threshold 60s). Local + remote notification suppressed."
        );
        assert_eq!(
            render(Some("no_transport"), true, None, None),
            "Host notification sent. Remote push not sent (Remote Control inactive)."
        );
        assert_eq!(
            render(Some("no_transport"), false, None, None),
            "Remote push not sent (Remote Control inactive)."
        );
        assert_eq!(
            render(None, true, None, None),
            "Host notification sent. Remote push requested."
        );
        assert_eq!(render(None, false, None, None), "Remote push requested.");
    }

    #[test]
    fn iso8601_shape() {
        // 0 → 1970-01-01T00:00:00.000Z (spot-check the civil-date math).
        // (now_iso8601 uses the real clock; assert the format via a known epoch.)
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
    }

    #[test]
    fn prompt_routine_addendum_gated_on_entrypoint() {
        let _g = guard();
        let t = PushNotificationTool::new(shell_test_ctx(dummy_out()));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let opts = PromptOptions::default();
        let base = rt.block_on(t.prompt(&opts));
        assert!(base.starts_with("This tool sends a host notification"));
        assert!(!base.contains("terminal"));
        assert!(!base.contains("scheduled routine"));
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "remote_trigger");
        let routine = rt.block_on(t.prompt(&opts));
        assert!(routine.contains("This is a scheduled routine"));
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
    }
}
