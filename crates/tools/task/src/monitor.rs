//! `MonitorTool` — the background-process event-monitor tool (binary `EVp`/`bVp`,
//! name `IA = "Monitor"`).
//!
//! Arms a background monitor on a shell `command`; in the binary each stdout line
//! becomes a live `TaskNotification` (token-bucket rate-limited, with suppression),
//! bounded by `timeout_ms` (default 5m, capped to 30m, or 10m in single-prompt
//! mode). The legacy persistent branch remains behind the rollout kill switch.
//!
//! **Gated off by default.** `isEnabled = Eq() && mu()` where
//! `Eq() = nt("tengu_amber_sentinel", false)` (no live GrowthBook → false) and
//! `mu()` = shell-available. So the tool is registered-but-disabled (invisible to
//! the model), byte-identical to the shipped binary — exactly like `PushNotification`.
//!
//! The implementation preserves the public static surface — name,
//! `description()`/`prompt()` (both `cJr + lJr()`, the latter the `Yke()`-gated
//! PushNotification splice), the input/output schemas, `isEnabled`, permission,
//! descriptor — and dispatches a real registry-backed monitor. The task runner
//! streams stdout lines into rate-limited live notifications, spools stderr only,
//! and remains addressable through `TaskOutput` / `TaskStop`.

use async_trait::async_trait;
use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use lingxi_core::host::task_registry::{MonitorRegistration, WebSocketMonitorRegistration};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, source_text_projection,
    source_text_projection_units, DescriptionOptions, PromptOptions, Tool, ToolCallResult,
    ToolError, ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance,
    ToolStaticContext, ValidationError,
};
use tool_api::BuiltinToolContext;

/// Binary `IA` — the tool name.
pub const MONITOR_TOOL_NAME: &str = "Monitor";

/// Binary `Z8` — the PushNotification name interpolated into `lJr()`.
const PUSH_NOTIFICATION_NAME: &str = "PushNotification";

/// Binary `tengu_amber_sentinel` — the `Eq()` isEnabled flag.
const AMBER_SENTINEL_FLAG: &str = "tengu_amber_sentinel";

/// Binary `Lnl` — default `timeout_ms` (5 minutes).
const DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// Binary `cEo` — max `timeout_ms` (1 hour).
const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Binary `Onl` — the CCR (remote) timeout cap (30 minutes).
const CCR_TIMEOUT_CAP_MS: u64 = 1_800_000;

/// Binary `cJr` — the Monitor description (byte-exact). Spliced with `lJr()` for
/// both `description()` and `prompt()`.
/// claude-code `Sbn()` — the Monitor description, which has TWO spots that
/// change when background tasks are disabled (`Dl()`). Upstream splices them
/// inline; here they are named so the two variants are visible side by side.
///
/// Everything else in the description is identical between the two.
const CJR_HEAD: &str = r#"Start a background monitor that streams events from a long-running script. Each stdout line is an event — you keep working and notifications arrive in the chat. Events arrive on their own schedule and are not replies from the user, even if one lands while you're waiting for the user to answer a question.

Pick by how many notifications you need:
- **One** ("tell me when the server is ready / the build finishes") → "#;
/// `Dl()` false — the default: point at `run_in_background`.
const CJR_ONE_SHOT_BACKGROUND: &str = r#"use **Bash with `run_in_background`** and a command that exits when the condition is true, e.g. `until grep -q "Ready in" dev.log; do sleep 0.5; done`. You get a single completion notification when it exits."#;
/// `Dl()` true — background tasks are off, so the same job is a foreground
/// Bash loop.
const CJR_ONE_SHOT_FOREGROUND: &str = r#"run the command in the **foreground with Bash**, exiting when the condition is true, e.g. `until grep -q "Ready in" dev.log; do sleep 0.5; done`."#;
const CJR_MID: &str = r#"
- **One per occurrence, indefinitely** ("tell me every time an ERROR line appears") → Monitor with an unbounded command (`tail -f`, `inotifywait -m`, `while true`).
- **One per occurrence, until a known end** ("emit each CI step result, stop when the run completes") → Monitor with a command that emits lines and then exits.

Your script's stdout is the event stream. Each line becomes a notification. Exit ends the watch.

  # Each matching log line is an event
  tail -f /var/log/app.log | grep --line-buffered "ERROR"

  # Each file change is an event
  inotifywait -m --format '%e %f' /watched/dir

  # Poll GitHub for new PR comments and emit one line per new comment
  last=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  while true; do
    now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    gh api "repos/owner/repo/issues/123/comments?since=$last" --jq '.[] | "\(.user.login): \(.body)"'
    last=$now; sleep 30
  done

  # Node script that emits events as they arrive (e.g. WebSocket listener)
  node watch-for-events.js

  # Per-occurrence with a natural end: emit each CI check as it lands, exit when the run completes
  prev=""
  while true; do
    s=$(gh pr checks 123 --json name,bucket)
    cur=$(jq -r '.[] | select(.bucket!="pending") | "\(.name): \(.bucket)"' <<<"$s" | sort)
    comm -13 <(echo "$prev") <(echo "$cur")
    prev=$cur
    jq -e 'all(.bucket!="pending")' <<<"$s" >/dev/null && break
    sleep 30
  done

**Don't use an unbounded command for a single notification.** `tail -f`, `inotifywait -m`, and `while true` never exit on their own, so the monitor stays armed until timeout even after the event has fired. For "tell me when X is ready," "#;
/// The same choice again, in the "unbounded command" warning.
const CJR_UNBOUNDED_BACKGROUND: &str = r#"use Bash `run_in_background` with an `until` loop instead (one notification, ends in seconds)"#;
const CJR_UNBOUNDED_FOREGROUND: &str = r#"use a foreground Bash `until` loop instead"#;
const CJR_TAIL: &str = r#". Note that `tail -f log | grep -m 1 ...` does *not* fix this: if the log goes quiet after the match, `tail` never receives SIGPIPE and the pipeline hangs anyway.

**Script quality:**
- Every pipe stage must flush per line or matches sit in its buffer unseen: `grep` needs `--line-buffered`, `awk` needs `fflush()`. `head` cannot flush at all — `| head -N` delivers nothing until N matches accumulate, then ends the stream.
- In poll loops, handle transient failures (`curl ... || true`) — one failed request shouldn't kill the monitor.
- Poll intervals: 30s+ for remote APIs (rate limits), 0.5-1s for local checks.
- Write a specific `description` — it appears in every notification ("errors in deploy.log" not "watching logs").
- Only stdout is the event stream. Stderr goes to the output file (readable via Read) but does not trigger notifications — for a command you run directly (e.g. `python train.py 2>&1 | grep --line-buffered ...`), merge stderr with `2>&1` so its failures reach your filter. (No effect on `tail -f` of an existing log — that file only contains what its writer redirected.)

**Coverage — silence is not success.** When watching a job or process for an outcome, your filter must match every terminal state, not just the happy path. A monitor that greps only for the success marker stays silent through a crashloop, a hung process, or an unexpected exit — and silence looks identical to "still running." Before arming, ask: *if this process crashed right now, would my filter emit anything?* If not, widen it.

  # Wrong — silent on crash, hang, or any non-success exit
  tail -f run.log | grep --line-buffered "elapsed_steps="

  # Right — one alternation covering progress + the failure signatures you'd act on
  tail -f run.log | grep -E --line-buffered "elapsed_steps=|Traceback|Error|FAILED|assert|Killed|OOM"

For poll loops checking job state, emit on every terminal status (`succeeded|failed|cancelled|timeout`), not just success. If you cannot confidently enumerate the failure signatures, broaden the grep alternation rather than narrow it — some extra noise is better than missing a crashloop.

**Output volume**: Every stdout line is a conversation message, so the filter should be selective — but selective means "the lines you'd act on," not "only good news." Never pipe raw logs; filter to exactly the success and failure signals you care about. Monitors that produce too many events are automatically stopped; restart with a tighter filter if this happens.

Stdout lines within 200ms are batched into a single notification, so multiline output from a single event groups naturally.

The script runs in the same shell environment as Bash. Exit ends the watch (exit code is reported). Timeout → killed. Set `persistent: true` for session-length watches (PR monitoring, log tails) — the monitor runs until you call TaskStop or the session ends. Use TaskStop to cancel early."#;

const WS_DESCRIPTION: &str = r#"
**ws source** — open a WebSocket and stream each incoming text frame as an event. No shell, no polling: the server pushes, you get notified.

  Monitor({
    ws: {url: 'wss://events.example.com/stream', protocols: ['v1']},
    description: 'deploy events',
  })

Each text frame becomes one notification (multiline frames stay as one event). Binary frames are reported as `[binary frame, N bytes]` rather than passed through. Socket close ends the watch with the close code surfaced; errors are surfaced before close. Same rate limiting as bash — a firehose will be suppressed and eventually stopped, so subscribe to a filtered feed where one exists.

Prefer this over `command: 'websocat wss://…'` — it avoids the extra process and line-buffering pitfalls. Use bash when you need to transform or filter frames with shell tools before they become events."#;

#[path = "monitor_schema.rs"]
mod native_schema;

fn websocket_host(ws: JsonValueView<'_>) -> Result<String, &'static str> {
    const INVALID_URL: &str =
        "url must be a valid ASCII ws:// or wss:// URL with no userinfo or whitespace";
    let raw = ws
        .get_property("url")
        .and_then(JsonValueView::as_str)
        .ok_or(INVALID_URL)?;
    if native_schema::hidden_control(raw) {
        return Err(INVALID_URL);
    }
    let host = permission::monitor_websocket_url_host(raw).ok_or(INVALID_URL)?;
    if let Some(protocols) = ws.get_property("protocols") {
        let values = protocols.array_len().ok_or("protocols must be an array")?;
        let mut seen = std::collections::HashSet::new();
        for index in 0..values {
            let token = protocols
                .get_index(index)
                .and_then(JsonValueView::as_str)
                .unwrap_or("");
            if token.is_empty()
                || !token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            {
                return Err("protocol must be an RFC 6455 token");
            }
            if !seen.insert(token) {
                return Err("protocols must be unique");
            }
        }
    }
    Ok(host.to_ascii_lowercase())
}

/// Assemble the description for the current background-tasks setting.
fn cjr() -> String {
    let disabled = lingxi_core::host::env::background_tasks_disabled();
    let (one_shot, unbounded) = if disabled {
        (CJR_ONE_SHOT_FOREGROUND, CJR_UNBOUNDED_FOREGROUND)
    } else {
        (CJR_ONE_SHOT_BACKGROUND, CJR_UNBOUNDED_BACKGROUND)
    };
    let tail = if cron::bounded_monitors_enabled() {
        CJR_TAIL.replace("Timeout → killed. Set `persistent: true` for session-length watches (PR monitoring, log tails) — the monitor runs until you call TaskStop or the session ends.", &format!("Every monitor expires after `timeout_ms` (default 5 minutes, at most {} minutes): it is killed and you get one notice with the event count. Re-arm it if you still need the watch; for a long watch (PR monitoring, log tails) set `timeout_ms` to the maximum and re-arm on each expiry, and widen the filter if an expiry with no events was unexpected.", cron::bounded_monitor_timeout_ms() / 60_000))
    } else {
        CJR_TAIL.into()
    };
    format!("{CJR_HEAD}{one_shot}{CJR_MID}{unbounded}{tail}")
}

/// 2.1.263 `ybn()` (`src_160113288.js` @1041; `lJr()` in the older builds) —
/// the `oJ()`-gated PushNotification addendum spliced onto BOTH `description()`
/// and `prompt()`. Reuses `cron::is_push_notif_enabled` (the exact predicate).
///
/// TWO leading newlines, not one: the oracle returns
/// `` `\n\nWhen an event lands…` `` so the addendum starts its own paragraph
/// after the preceding section. With one it ran on as the next line of that
/// section.
fn ljr() -> String {
    if cron::is_push_notif_enabled() {
        format!(
            "\n\nWhen an event lands that the user would want to act on now \u{2014} an error appeared, the status they were waiting on flipped \u{2014} send a {PUSH_NOTIFICATION_NAME}. Not every event is worth a push; the ones that change what they'd do next are."
        )
    } else {
        String::new()
    }
}

/// 2.1.263 `QI()` = `H("tengu_amber_sentinel", false)` (port:
/// `telemetry::flag_bool`). Spelled `Eq()` in the 2.1.2xx builds this module
/// was first written against.
fn amber_sentinel_enabled() -> bool {
    telemetry::flag_bool(AMBER_SENTINEL_FLAG, false)
}

/// 2.1.263 `Ys()` (`src_160256736.js` @17607), the second half of the Monitor
/// gate — spelled `mu()` in the older builds:
///
/// ```js
/// function Ys(){if(P()!=="windows")return!0;return _1()!==null}
/// ```
///
/// NOT unconditional: on Windows the tool is withdrawn unless a bash can be
/// found. `_1()` is the Git-Bash discovery the Bash tool already ports as
/// [`lingxi_core::host::shell_support::git_bash_path`], so this reuses it instead of repeating
/// the probe order (env override → Program Files → git-on-PATH). Every other
/// platform answers `true`, which is why the previous unconditional `true` was
/// right everywhere except a Windows host with no Git Bash — there it offered a
/// tool whose commands could not run.
fn shell_available() -> bool {
    if !cfg!(windows) {
        return true;
    }
    lingxi_core::host::shell_support::git_bash_path().is_some()
}

/// Binary `Mnl` (`applyCcrTimeoutCap`): under `LINGXI_REMOTE` a persistent
/// monitor is capped to a 30-minute timeout (persistent→false); otherwise the
/// requested `(timeout_ms, persistent)` pass through unchanged.
#[cfg(test)]
fn apply_ccr_timeout_cap(timeout_ms: u64, persistent: bool) -> (u64, bool) {
    let (timeout, persistent) = apply_ccr_timeout_cap_number(timeout_ms as f64, persistent);
    (timeout as u64, persistent)
}

fn apply_ccr_timeout_cap_number(timeout_ms: f64, persistent: bool) -> (f64, bool) {
    if cron::bounded_monitors_enabled() {
        return (
            timeout_ms.min(cron::bounded_monitor_timeout_ms() as f64),
            false,
        );
    }
    // Upstream tests the environment string directly, so even "0" is truthy.
    let remote = std::env::var_os("LINGXI_REMOTE").is_some_and(|value| !value.is_empty());
    if !remote {
        return (timeout_ms, persistent);
    }
    if persistent {
        (CCR_TIMEOUT_CAP_MS as f64, false)
    } else {
        (timeout_ms.min(CCR_TIMEOUT_CAP_MS as f64), false)
    }
}

/// Binary `yVp` — the input schema (`hVp` + `command`). Byte-exact field describes.
static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "description": {
                "type": "string",
                "description": "Short human-readable description of what you are monitoring (shown in notifications)."
            },
            "timeout_ms": {
                "type": "number",
                "minimum": 1000,
                "default": DEFAULT_TIMEOUT_MS,
                "description": "Kill the monitor after this deadline. Default 300000ms, max 3600000ms. Ignored when persistent is true."
            },
            "persistent": {
                "type": "boolean",
                "default": false,
                "description": "Run for the lifetime of the session (no timeout). Use for session-length watches like PR monitoring or log tails. Stop with TaskStop."
            },
            "command": {
                "type": "string",
                "description": "Shell command or script. Each stdout line is an event; exit ends the watch."
            },
            "ws": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "url": {"type": "string"},
                    "protocols": {"type": "array", "items": {"type": "string", "pattern": "^[!#$%&'*+.^_`|~0-9A-Za-z-]+$"}}
                },
                "required": ["url"],
                "description": "WebSocket to open. Each text frame is an event; binary frames are reported as a placeholder line. Socket close ends the watch. Cannot be combined with command."
            }
        },
        "required": ["description", "timeout_ms", "persistent"]
    })
});

fn bounded_input_schema(max_ms: u64) -> Value {
    let mut schema = INPUT_SCHEMA.clone();
    schema["required"] = json!(["description", "timeout_ms"]);
    schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("persistent");
    schema["properties"]["timeout_ms"]["maximum"] = json!(MAX_TIMEOUT_MS);
    schema["properties"]["timeout_ms"]["description"] = json!(format!("Kill the monitor after this deadline. Default 300000ms. Deadlines above {max_ms}ms are capped to {max_ms}ms. You are notified at expiry and can re-arm."));
    schema
}

static BOUNDED_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| bounded_input_schema(1_800_000));
static SINGLE_SHOT_INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| bounded_input_schema(600_000));

static SOURCE_INPUT_SCHEMA: Lazy<Utf16JsonProjection> =
    Lazy::new(|| generated_builtin_schema(&INPUT_SCHEMA));
static SOURCE_BOUNDED_INPUT_SCHEMA: Lazy<Utf16JsonProjection> =
    Lazy::new(|| generated_builtin_schema(&BOUNDED_INPUT_SCHEMA));
static SOURCE_SINGLE_SHOT_INPUT_SCHEMA: Lazy<Utf16JsonProjection> =
    Lazy::new(|| generated_builtin_schema(&SINGLE_SHOT_INPUT_SCHEMA));

static SOURCE_OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> =
    Lazy::new(|| generated_builtin_schema(&OUTPUT_SCHEMA));

fn monitor_custom_error(
    path: Vec<Utf16JsonProjection>,
    message: &str,
) -> tool_api::native_schema::NativeSchemaError {
    monitor_schema_error(vec![monitor_issue(vec![
        ("code", source_text_projection("custom")),
        ("path", monitor_path(path)),
        ("message", source_text_projection(message)),
    ])])
}

fn monitor_source_type(value: Option<JsonValueView<'_>>) -> &'static str {
    let Some(value) = value else {
        return "undefined";
    };
    if value.is_null() {
        "null"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.as_number().is_some() {
        "number"
    } else if value.code_units().is_some() {
        "string"
    } else if value.array_len().is_some() {
        "array"
    } else {
        "object"
    }
}

fn hidden_control_units(units: &[u16]) -> bool {
    units.iter().any(|unit| {
        *unit != u16::from(b'\t')
            && *unit != u16::from(b'\n')
            && (*unit < 32 || (127..=159).contains(unit))
    })
}

fn valid_protocol_units(units: &[u16]) -> bool {
    !units.is_empty()
        && units.iter().all(|unit| {
            *unit <= 0x7f
                && ((*unit as u8).is_ascii_alphanumeric()
                    || b"!#$%&'*+.^_`|~-".contains(&(*unit as u8)))
        })
}

fn empty_source_string() -> Utf16JsonProjection {
    source_text_projection("")
}

fn text_encoder_string(value: &Utf16JsonProjection) -> String {
    // Native TextEncoder replaces unpaired UTF-16 surrogates with U+FFFD at
    // the point command text is prepared for process execution. Keep the
    // original UJP in MonitorRegistration for task/notification metadata.
    let units = value
        .string_units("")
        .expect("Monitor command is a source string");
    String::from_utf16_lossy(&units)
}

fn monitor_issue(fields: Vec<(&str, Utf16JsonProjection)>) -> Utf16JsonProjection {
    Utf16JsonProjection::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
    .expect("Monitor schema issue is valid JSON")
}

fn monitor_path(parts: Vec<Utf16JsonProjection>) -> Utf16JsonProjection {
    Utf16JsonProjection::array(parts).expect("Monitor issue path is valid JSON")
}

fn monitor_type_issue(
    expected: &str,
    value: Option<JsonValueView<'_>>,
    path: Vec<Utf16JsonProjection>,
) -> Utf16JsonProjection {
    monitor_issue(vec![
        ("expected", source_text_projection(expected)),
        ("code", source_text_projection("invalid_type")),
        ("path", monitor_path(path)),
        (
            "message",
            source_text_projection(&format!(
                "Invalid input: expected {expected}, received {}",
                monitor_source_type(value)
            )),
        ),
    ])
}

fn monitor_schema_error(
    issues: Vec<Utf16JsonProjection>,
) -> tool_api::native_schema::NativeSchemaError {
    let issues_value = Utf16JsonProjection::array(issues.clone())
        .expect("Monitor schema issue array is valid JSON");
    let raw = issues_value
        .to_pretty_json_string()
        .expect("Monitor schema issues stringify as JSON");
    tool_api::native_schema::NativeSchemaError {
        display: tool_api::native_schema::format_issues("Monitor", &issues),
        raw: source_text_projection(&raw),
    }
}

fn parse_monitor_input(
    input: &Utf16JsonProjection,
    bounded: bool,
) -> Result<Utf16JsonProjection, tool_api::native_schema::NativeSchemaError> {
    let input =
        tool_api::native_schema::normalize_flat_input(input).unwrap_or_else(|| input.clone());
    let Some(root) = input.at_pointer("") else {
        return Err(monitor_schema_error(vec![monitor_type_issue(
            "object",
            None,
            vec![],
        )]));
    };
    let Some(entries) = root.object_entries() else {
        return Err(monitor_schema_error(vec![monitor_type_issue(
            "object",
            Some(root),
            vec![],
        )]));
    };

    let mut issues = Vec::<Utf16JsonProjection>::new();
    let mut parsed = Vec::<(String, Utf16JsonProjection)>::new();
    let mut parsed_timeout = DEFAULT_TIMEOUT_MS as f64;
    let mut parsed_persistent = false;
    let mut has_command = false;
    let mut has_ws = false;

    let description = root.get_property("description");
    if description.and_then(JsonValueView::code_units).is_none() {
        issues.push(monitor_type_issue(
            "string",
            description,
            vec![source_text_projection("description")],
        ));
    } else {
        parsed.push((
            "description".into(),
            input
                .subprojection("/description")
                .expect("string description is present"),
        ));
    }

    let timeout = root.get_property("timeout_ms");
    if let Some(value) = timeout {
        if let Some(number) = value.as_number() {
            parsed_timeout = number;
            parsed.push((
                "timeout_ms".into(),
                input
                    .subprojection("/timeout_ms")
                    .expect("number timeout is present"),
            ));
            if number < 1000.0 {
                issues.push(monitor_issue(vec![
                    ("origin", source_text_projection("number")),
                    ("code", source_text_projection("too_small")),
                    ("minimum", generated_builtin_result(&json!(1000))),
                    ("inclusive", generated_builtin_result(&json!(true))),
                    (
                        "path",
                        monitor_path(vec![source_text_projection("timeout_ms")]),
                    ),
                    (
                        "message",
                        source_text_projection("Too small: expected number to be >=1000"),
                    ),
                ]));
            }
            if bounded && number > MAX_TIMEOUT_MS as f64 {
                issues.push(monitor_issue(vec![
                    ("origin", source_text_projection("number")),
                    ("code", source_text_projection("too_big")),
                    ("maximum", generated_builtin_result(&json!(MAX_TIMEOUT_MS))),
                    ("inclusive", generated_builtin_result(&json!(true))),
                    (
                        "path",
                        monitor_path(vec![source_text_projection("timeout_ms")]),
                    ),
                    (
                        "message",
                        source_text_projection("timeout_ms must be ≤ 3600000"),
                    ),
                ]));
            }
        } else {
            issues.push(monitor_type_issue(
                "number",
                Some(value),
                vec![source_text_projection("timeout_ms")],
            ));
        }
    } else {
        parsed.push((
            "timeout_ms".into(),
            generated_builtin_result(&json!(DEFAULT_TIMEOUT_MS)),
        ));
    }

    if !bounded {
        let persistent = root.get_property("persistent");
        if let Some(value) = persistent {
            if let Some(value) = value.as_bool() {
                parsed_persistent = value;
                parsed.push((
                    "persistent".into(),
                    input
                        .subprojection("/persistent")
                        .expect("boolean persistent is present"),
                ));
            } else {
                issues.push(monitor_type_issue(
                    "boolean",
                    Some(value),
                    vec![source_text_projection("persistent")],
                ));
            }
        } else {
            parsed.push(("persistent".into(), generated_builtin_result(&json!(false))));
        }
    }

    if let Some(command) = root.get_property("command") {
        if let Some(command_units) = command.code_units() {
            has_command = !command_units.is_empty();
            parsed.push((
                "command".into(),
                input
                    .subprojection("/command")
                    .expect("string command is present"),
            ));
            if hidden_control_units(command_units) {
                issues.push(monitor_issue(vec![
                    ("code", source_text_projection("custom")),
                    (
                        "path",
                        monitor_path(vec![source_text_projection("command")]),
                    ),
                    (
                        "message",
                        source_text_projection(
                            "command contains control characters that would be hidden in the approval dialog",
                        ),
                    ),
                ]));
            }
        } else {
            issues.push(monitor_type_issue(
                "string",
                Some(command),
                vec![source_text_projection("command")],
            ));
        }
    }

    if let Some(ws) = root.get_property("ws") {
        has_ws = true;
        if ws.object_entries().is_none() {
            issues.push(monitor_type_issue(
                "object",
                Some(ws),
                vec![source_text_projection("ws")],
            ));
        } else {
            let mut ws_fields = Vec::<(String, Utf16JsonProjection)>::new();
            let url = ws.get_property("url");
            if let Some(url_units) = url.and_then(JsonValueView::code_units) {
                ws_fields.push((
                    "url".into(),
                    input
                        .subprojection("/ws/url")
                        .expect("string ws URL is present"),
                ));
                if hidden_control_units(url_units) {
                    issues.push(monitor_issue(vec![
                        ("code", source_text_projection("custom")),
                        (
                            "path",
                            monitor_path(vec![
                                source_text_projection("ws"),
                                source_text_projection("url"),
                            ]),
                        ),
                        (
                            "message",
                            source_text_projection(
                                "url contains control characters that would be hidden in the approval dialog",
                            ),
                        ),
                    ]));
                }
                if url.and_then(JsonValueView::as_str).is_none_or(|url_value| {
                    permission::monitor_websocket_url_host(url_value).is_none()
                }) {
                    issues.push(monitor_issue(vec![
                        ("code", source_text_projection("custom")),
                        (
                            "path",
                            monitor_path(vec![
                                source_text_projection("ws"),
                                source_text_projection("url"),
                            ]),
                        ),
                        (
                            "message",
                            source_text_projection(
                                "url must be a valid ASCII ws:// or wss:// URL with no userinfo or whitespace",
                            ),
                        ),
                    ]));
                }
            } else {
                issues.push(monitor_type_issue(
                    "string",
                    url,
                    vec![source_text_projection("ws"), source_text_projection("url")],
                ));
            }

            if let Some(protocols) = ws.get_property("protocols") {
                if let Some(count) = protocols.array_len() {
                    let mut strings_only = true;
                    let mut seen = std::collections::HashSet::<Vec<u16>>::new();
                    for index in 0..count {
                        let protocol = protocols.get_index(index).expect("array item exists");
                        let path = vec![
                            source_text_projection("ws"),
                            source_text_projection("protocols"),
                            generated_builtin_result(&json!(index)),
                        ];
                        if let Some(token_units) = protocol.code_units() {
                            if !valid_protocol_units(token_units) {
                                issues.push(monitor_issue(vec![
                                    ("origin", source_text_projection("string")),
                                    ("code", source_text_projection("invalid_format")),
                                    ("format", source_text_projection("regex")),
                                    (
                                        "pattern",
                                        source_text_projection("/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/"),
                                    ),
                                    ("path", monitor_path(path)),
                                    (
                                        "message",
                                        source_text_projection(
                                            "protocol must be an RFC 6455 token",
                                        ),
                                    ),
                                ]));
                            }
                            seen.insert(token_units.to_vec());
                        } else {
                            strings_only = false;
                            issues.push(monitor_type_issue("string", Some(protocol), path));
                        }
                    }
                    if strings_only && seen.len() != count {
                        issues.push(monitor_issue(vec![
                            ("code", source_text_projection("custom")),
                            (
                                "path",
                                monitor_path(vec![
                                    source_text_projection("ws"),
                                    source_text_projection("protocols"),
                                ]),
                            ),
                            (
                                "message",
                                source_text_projection("protocols must be unique"),
                            ),
                        ]));
                    }
                    ws_fields.push((
                        "protocols".into(),
                        input
                            .subprojection("/ws/protocols")
                            .expect("protocol array is present"),
                    ));
                } else {
                    issues.push(monitor_type_issue(
                        "array",
                        Some(protocols),
                        vec![
                            source_text_projection("ws"),
                            source_text_projection("protocols"),
                        ],
                    ));
                }
            }
            if !ws_fields.is_empty() {
                parsed.push((
                    "ws".into(),
                    Utf16JsonProjection::object(ws_fields)
                        .expect("parsed WebSocket fields form an object"),
                ));
            }
        }
    }

    if !bounded {
        let mut unknown = entries
            .iter()
            .filter(|entry| {
                ![
                    "description",
                    "timeout_ms",
                    "persistent",
                    "command",
                    "ws",
                    "__proto__",
                ]
                .iter()
                .any(|key| entry.code_units == key.encode_utf16().collect::<Vec<_>>())
            })
            .map(|entry| entry.code_units.to_vec())
            .collect::<Vec<_>>();
        unknown.sort_by_key(|key| {
            String::from_utf16(key)
                .ok()
                .and_then(|key| key.parse::<u32>().ok())
                .filter(|value| {
                    *value != u32::MAX
                        && value.to_string().encode_utf16().collect::<Vec<_>>() == *key
                })
                .map_or((1, 0), |value| (0, value))
        });
        if !unknown.is_empty() {
            let mut message = format!(
                "Unrecognized key{}: ",
                if unknown.len() == 1 { "" } else { "s" }
            )
            .encode_utf16()
            .collect::<Vec<_>>();
            for (index, key) in unknown.iter().enumerate() {
                if index > 0 {
                    message.extend(", ".encode_utf16());
                }
                message.extend("\"".encode_utf16());
                message.extend(key);
                message.extend("\"".encode_utf16());
            }
            issues.push(monitor_issue(vec![
                ("code", source_text_projection("unrecognized_keys")),
                (
                    "keys",
                    Utf16JsonProjection::array(
                        unknown
                            .iter()
                            .map(|key| source_text_projection_units(key))
                            .collect(),
                    )
                    .expect("Monitor unknown-key list is valid JSON"),
                ),
                ("path", monitor_path(Vec::new())),
                ("message", source_text_projection_units(&message)),
            ]));
        }
    }

    let aborted = issues.iter().any(|issue| {
        matches!(
            issue.string_at("/code"),
            Some("invalid_type" | "unrecognized_keys")
        )
    });
    if !aborted {
        if has_command == has_ws {
            issues.push(monitor_issue(vec![
                ("code", source_text_projection("custom")),
                ("path", monitor_path(Vec::new())),
                (
                    "message",
                    source_text_projection("exactly one of command or ws"),
                ),
            ]));
        }
        if !bounded && !parsed_persistent && parsed_timeout > MAX_TIMEOUT_MS as f64 {
            issues.push(monitor_issue(vec![
                ("code", source_text_projection("custom")),
                (
                    "path",
                    monitor_path(vec![source_text_projection("timeout_ms")]),
                ),
                (
                    "message",
                    source_text_projection("timeout_ms must be ≤ 3600000"),
                ),
            ]));
        }
    }
    if !issues.is_empty() {
        return Err(monitor_schema_error(issues));
    }
    Utf16JsonProjection::object(parsed)
        .map_err(|_| monitor_custom_error(vec![], "could not construct parsed Monitor input"))
}

fn started_message(task_id: &str, timeout_ms: f64, persistent: bool) -> String {
    let deadline = if persistent {
        "persistent — runs until TaskStop or session end".into()
    } else if cron::bounded_monitors_enabled() {
        format!("expires in {} unless the source ends first; you get one notice at expiry — re-arm if you still need the watch", cron::monitor_duration(timeout_ms as u64))
    } else {
        format!(
            "timeout {}ms",
            generated_builtin_result(&json!(timeout_ms))
                .to_json_string()
                .expect("generated timeout is valid JSON")
        )
    };
    format!("Monitor started (task {task_id}, {deadline}). You will be notified on each event. Keep working — do not poll or sleep. Events may arrive while you are waiting for the user — an event is not their reply.")
}

/// Binary `TVp` — the output schema.
static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "taskId": { "type": "string", "description": "ID of the background monitor task." },
            "timeoutMs": { "type": "number", "description": "Timeout deadline in milliseconds (0 when persistent)." },
            "persistent": { "type": "boolean", "description": "No timeout — runs until TaskStop or session end." }
        },
        "required": ["taskId", "timeoutMs"]
    })
});

/// `Monitor` — arm a background event monitor (binary `EVp`).
pub struct MonitorTool {
    ctx: BuiltinToolContext,
}

impl MonitorTool {
    /// Construct the tool over the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        tool_api::builtin_context::install_shell_discovery_logging();
        Self { ctx }
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        MONITOR_TOOL_NAME
    }

    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("watch, monitor, or keep an eye on a process/log/command or WebSocket — stream each stdout line as a live notification".into())
    }

    fn user_facing_name(&self) -> Option<&str> {
        Some(MONITOR_TOOL_NAME)
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        if !cron::bounded_monitors_enabled() {
            &SOURCE_INPUT_SCHEMA
        } else if cron::bounded_monitor_timeout_ms() == 600_000 {
            &SOURCE_SINGLE_SHOT_INPUT_SCHEMA
        } else {
            &SOURCE_BOUNDED_INPUT_SCHEMA
        }
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn native_input_validation(&self) -> bool {
        true
    }

    fn parse_native_input(
        &self,
        input: &Utf16JsonProjection,
    ) -> Option<Result<Utf16JsonProjection, tool_api::native_schema::NativeSchemaError>> {
        Some(parse_monitor_input(input, cron::bounded_monitors_enabled()))
    }

    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&SOURCE_OUTPUT_SCHEMA)
    }

    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        tool_api::native_output::parse_object(
            output,
            MONITOR_TOOL_NAME,
            &[
                tool_api::native_output::OutputField {
                    name: "taskId",
                    kind: tool_api::native_output::OutputFieldType::String,
                    required: true,
                },
                tool_api::native_output::OutputField {
                    name: "timeoutMs",
                    kind: tool_api::native_output::OutputFieldType::Number,
                    required: true,
                },
                tool_api::native_output::OutputField {
                    name: "persistent",
                    kind: tool_api::native_output::OutputFieldType::Boolean,
                    required: false,
                },
            ],
            tool_api::native_output::UnknownOutputKeys::Strip,
        )
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // PARITY: 2.1.263 `isEnabled(){return QI()&&Ys()}`
        // (`src_168769646.js` @12008) — default OFF.
        amber_sentinel_enabled() && shell_available()
    }

    fn should_defer(&self) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        10_000
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        // Monitor spawns a command (side effects) — not read-only.
        false
    }

    async fn validate_input(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        // Structural checks/refinements belong to safeParse, before this hook.
        Ok(())
    }

    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        if let Some(ws) = input.get_property("ws") {
            let host = match websocket_host(ws) {
                Ok(host) => host,
                Err(message) => {
                    return PermissionResult::Deny {
                        reason: PermissionDecisionReason::Other {
                            reason: message.into(),
                        },
                        explanation: Some(message.into()),
                        metadata: PermissionMetadata::default(),
                    }
                }
            };
            if host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| !lingxi_core::host::http::is_public_monitor_address(ip))
            {
                return PermissionResult::Deny {
                    reason: PermissionDecisionReason::Other { reason: "SSRF-blocked address range".into() },
                    explanation: Some(format!("Monitor cannot open a WebSocket to {host}: the address is in a private, link-local, or cloud-metadata range.").into()),
                    metadata: PermissionMetadata::default(),
                };
            }
            let policy = self.ctx.effective_sandbox_runtime().network;
            let matches = |domain: &String| {
                domain == "*"
                    || domain == &host
                    || domain
                        .strip_prefix("*.")
                        .is_some_and(|suffix| host.ends_with(&format!(".{suffix}")))
            };
            if policy.denied_domains.iter().any(matches)
                || (policy.allow_managed_domains_only
                    && !policy.allowed_domains.iter().any(matches))
            {
                return PermissionResult::Deny {
                    reason: PermissionDecisionReason::Other {
                        reason: "sandbox network policy".into(),
                    },
                    explanation: Some(
                        format!(
                            "Monitor cannot open a WebSocket to {host}: denied by network policy."
                        )
                        .into(),
                    ),
                    metadata: PermissionMetadata::default(),
                };
            }
            let suffix = ws
                .get_property("protocols")
                .and_then(|protocols| {
                    let len = protocols.array_len()?;
                    (len > 0).then_some((protocols, len))
                })
                .map(|(protocols, len)| {
                    // .270 RYe: bounded permission preview, not a JSON array.
                    let shown = (0..len.min(4))
                        .map(|index| {
                            let value = protocols
                                .get_index(index)
                                .and_then(JsonValueView::as_str)
                                .unwrap_or_default();
                            // Measure in the SAME unit the cut uses. `str::len`
                            // is bytes while `chars().take(48)` is scalar
                            // values, so a 20-character CJK protocol (60 bytes)
                            // took the truncating branch, kept every character,
                            // and still got an ellipsis claiming a truncation
                            // that never happened. The oracle's `.length > 48` /
                            // `.slice(0, 48)` are both UTF-16 units, which agree
                            // with chars for every non-astral protocol token.
                            let value = if value.chars().count() > 48 {
                                format!("{}…", value.chars().take(48).collect::<String>())
                            } else {
                                value.to_string()
                            };
                            format!("\"{value}\"")
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let rest = len.saturating_sub(4);
                    let overflow = if rest > 0 {
                        format!(" (+{rest} more)")
                    } else {
                        String::new()
                    };
                    format!(" (subprotocols: {shown}{overflow})")
                })
                .unwrap_or_default();
            let message = format!(
                "Monitor will open a WebSocket to {}{suffix}",
                ws.get_property("url")
                    .and_then(JsonValueView::as_str)
                    .unwrap_or("")
            );
            return PermissionResult::Ask {
                reason: PermissionDecisionReason::Other {
                    reason: message.clone().into(),
                },
                prompt: permission::result::PermissionPrompt {
                    title: "Monitor".into(),
                    message: message.into(),
                    options: vec!["Allow".into(), "Deny".into()],
                },
                pending_classifier_check: None,
                metadata: PermissionMetadata::default(),
            };
        }
        // The oracle routes a command-Monitor through the FULL Bash resolver
        // (`Lon({...e,command},t)`). In the port that routing lives in the
        // PERMISSION GATE, which rewrites the effective tool name "Monitor"→"Bash"
        // for a command-monitor (see `PermissionPolicy::authorize_with_mode`), so
        // deny/ask rules keyed `Bash(...)`, the bash-safety AST, and the `&`
        // downgrade ALL apply. This tool-level check therefore allows (like
        // BashTool's tool-level allow-all); the gate is the authoritative barrier.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "Monitor: command permission resolved as Bash by the gate".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        // PARITY: `description(){return cJr+lJr()}`.
        format!("{}{}{}", cjr(), WS_DESCRIPTION, ljr())
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        // PARITY: `prompt(){return cJr+lJr()}` (identical to description).
        format!("{}{}{}", cjr(), WS_DESCRIPTION, ljr()).into()
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // 2.1.266 `a0`: refuse while THIS agent's own stop is still completing
        // (see `agent_processes::mark_stop_pending`).
        if let Some(agent_id) = ctx.agent_id {
            if lingxi_core::host::agent_processes::is_stop_pending(&agent_id.to_string()) {
                return Err(invalid_tool_input(
                    lingxi_core::host::agent_processes::stop_pending_refusal("start monitors."),
                ));
            }
        }

        let description = input
            .subprojection("/description")
            .expect("validated description subtree");
        let requested_timeout = input
            .number_at("/timeout_ms")
            .unwrap_or(DEFAULT_TIMEOUT_MS as f64);
        let requested_persistent = input
            .at_pointer("/persistent")
            .and_then(JsonValueView::as_bool)
            .unwrap_or(false);

        // PARITY: `Mnl(t)` — CCR timeout cap.
        let (timeout_ms, persistent) =
            apply_ccr_timeout_cap_number(requested_timeout, requested_persistent);
        let reported_timeout = if persistent {
            generated_builtin_result(&json!(0))
        } else if timeout_ms.fract() == 0.0 {
            generated_builtin_result(&json!(timeout_ms as u64))
        } else {
            generated_builtin_result(&json!(timeout_ms))
        };

        if let Some(ws) = input.get_property("ws") {
            websocket_host(ws).map_err(invalid_tool_input)?;
            if input
                .string_units("/command")
                .is_some_and(|command| !command.is_empty())
            {
                return Err(invalid_tool_input("exactly one of command or ws"));
            }
            let registry = self.ctx.task_registry.as_ref().ok_or_else(|| {
                ToolError::Internal("Monitor: task registry is not configured".into())
            })?;
            self.ctx
                .http
                .preflight_monitor_websocket(
                    ws.get_property("url")
                        .and_then(JsonValueView::as_str)
                        .unwrap_or_default(),
                )
                .await
                .map_err(|error| invalid_tool_input(format!("Monitor: {error}")))?;
            let timeout_field = if persistent { 0 } else { timeout_ms as u64 };
            let task_id = registry
                .spawn_websocket_monitor(
                    WebSocketMonitorRegistration {
                        url: ws
                            .get_property("url")
                            .and_then(JsonValueView::as_str)
                            .unwrap_or_default()
                            .into(),
                        protocols: ws
                            .get_property("protocols")
                            .and_then(|protocols| protocols.array_len().map(|len| (protocols, len)))
                            .map(|(protocols, len)| {
                                (0..len)
                                    .filter_map(|index| {
                                        protocols
                                            .get_index(index)
                                            .and_then(JsonValueView::as_str)
                                            .map(str::to_owned)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        task: MonitorRegistration {
                            command: input
                                .subprojection("/command")
                                .unwrap_or_else(|_| empty_source_string()),
                            description,
                            timeout_ms: timeout_field,
                            persistent,
                            tool_use_id: ctx.tool_use_id.map(|id| id.to_string()),
                            creator_teammate_name: ctx.agent_name,
                            creator_team_name: ctx.team_name,
                            creator_agent_id: ctx.agent_id,
                            ..Default::default()
                        },
                    },
                    self.ctx.http.clone(),
                )
                .await
                .map_err(|error| ToolError::Internal(format!("Monitor: {error}")))?;
            return Ok(ToolCallResult {
                data: Utf16JsonProjection::object(vec![
                    ("taskId".into(), source_text_projection(&task_id)),
                    ("timeoutMs".into(), reported_timeout),
                    (
                        "persistent".into(),
                        generated_builtin_result(&json!(persistent)),
                    ),
                ])
                .expect("Monitor result is valid JSON"),
                model_content: Some(source_text_projection(&started_message(
                    &task_id, timeout_ms, persistent,
                ))),
                is_error: false,
                new_messages: vec![],
                subagent_hooks_fired: false,
                context_modifier: None,
                mcp_meta: None,
            });
        }

        let command_source = input
            .subprojection("/command")
            .expect("validated command subtree");
        let command = text_encoder_string(&command_source);
        let registry = self.ctx.task_registry.as_ref().ok_or_else(|| {
            ToolError::Internal("Monitor: task registry is not configured".into())
        })?;
        // claude-code's Monitor spawns through the SHARED shell entry point, so
        // it inherits that path's pre-spawn cwd check: `if(A.preSpawnError)
        // throw new R(A.preSpawnError, "Monitor: pre-spawn error (cwd/argv
        // redacted)")`. The port's Monitor does not go through that path, so
        // the check is spelled out here — without it a monitor is minted for a
        // directory that no longer exists and only fails later, as a dead task.
        //
        // NOTE the second argument to `R` is the TELEMETRY label; the model
        // sees `preSpawnError` itself. Emitting "Monitor: pre-spawn error
        // (cwd/argv redacted)" to the model would be byte-wrong.
        //
        // Recovery is silent, matching both the oracle and the Bash tool: the
        // upstream guard is `if(vr>0) return OD(recovered-message)`, where `vr`
        // is the INDEX of the recovery target that worked — falling back to the
        // FIRST candidate returns no error at all. This engine has exactly one
        // fallback (the tool workspace), which is that first candidate.
        let cwd = ctx.cwd.unwrap_or_else(|| self.ctx.cwd());
        let workspace = self.ctx.cwd();
        let cwd = if std::fs::canonicalize(&cwd).is_ok() {
            cwd
        } else if std::fs::canonicalize(&workspace).is_ok() {
            workspace
        } else {
            return Err(ToolError::Internal(format!(
                "Working directory \"{}\" no longer exists. Please restart Claude from an existing directory.",
                cwd.display()
            )));
        };
        // MON-09: claude-code runs a monitor through the SAME sandbox decision
        // as an ordinary shell call — `vV(e, signal, "bash", {…,
        // shouldUseSandbox: jS({command}), preventCwdChanges: !0, …})`
        // (`src_168769646.js` @9633). The port's Monitor has its own spawn
        // path, which bypassed the sandbox entirely with an audited exception,
        // so a monitor command ran with more reach than the identical command
        // typed into Bash.
        //
        // `dangerouslyDisableSandbox` is `false`: `jS`'s escape hatch reads
        // that off the CALL, and Monitor's schema has no such input.
        let sandbox_runtime = self.ctx.effective_sandbox_runtime();
        let decision = sandbox::decision::should_use_sandbox(
            &command,
            self.ctx.sandbox_available,
            false,
            sandbox_runtime.are_unsandboxed_commands_allowed(),
            &sandbox_runtime,
            // The SESSION workspace, as the Bash tool passes it — the policy is
            // scoped to the session, not to this monitor's cwd. Re-read rather
            // than reusing the local above, which the cwd recovery may have
            // consumed.
            self.ctx.cwd(),
        );
        let spawn_command = match decision {
            sandbox::decision::SandboxDecision::NoSandbox => None,
            sandbox::decision::SandboxDecision::Sandbox { .. } => {
                let shell = lingxi_core::host::shell_support::resolve_shell_path().to_string();
                match self
                    .ctx
                    .sandbox_runner
                    .wrap(
                        &command,
                        &sandbox_runtime,
                        self.ctx.platform,
                        Some(&shell),
                        Some(cwd.as_path()),
                    )
                    .await
                {
                    Ok(wrapped) => Some(wrapped),
                    // A refusal is the same error the Bash tool surfaces; a
                    // monitor that cannot be confined must not fall back to
                    // running unconfined.
                    Err(sandbox::wrap::SandboxWrapError::Unsupported(message)) => {
                        return Err(invalid_tool_input(message))
                    }
                    Err(sandbox::wrap::SandboxWrapError::SbplWrite(message)) => {
                        return Err(ToolError::Io(message.into()))
                    }
                }
            }
        };

        let timeout_field = if persistent { 0 } else { timeout_ms as u64 };
        let task_id = registry
            .spawn_monitor(MonitorRegistration {
                command: command_source,
                spawn_command,
                description,
                timeout_ms: timeout_field,
                persistent,
                cwd: Some(cwd.to_string_lossy().into_owned()),
                tool_use_id: ctx.tool_use_id.map(|id| id.to_string()),
                creator_teammate_name: ctx.agent_name,
                creator_team_name: ctx.team_name,
                creator_agent_id: ctx.agent_id,
            })
            .await
            .map_err(|e| ToolError::Internal(format!("Monitor: {e}")))?;

        let model_content = started_message(&task_id, timeout_ms, persistent);

        Ok(ToolCallResult {
            data: Utf16JsonProjection::object(vec![
                ("taskId".into(), source_text_projection(&task_id)),
                ("timeoutMs".into(), reported_timeout),
                (
                    "persistent".into(),
                    generated_builtin_result(&json!(persistent)),
                ),
            ])
            .expect("Monitor result is valid JSON"),
            model_content: Some(source_text_projection(&model_content)),
            is_error: false,
            new_messages: vec![],
            context_modifier: None,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

#[cfg(test)]
#[derive(Debug)]
struct TestMonitorCallResult {
    data: Value,
    model_content: Option<String>,
    is_error: bool,
}

#[cfg(test)]
impl MonitorTool {
    fn input_schema(&self) -> &Value {
        if !cron::bounded_monitors_enabled() {
            &INPUT_SCHEMA
        } else if cron::bounded_monitor_timeout_ms() == 600_000 {
            &SINGLE_SHOT_INPUT_SCHEMA
        } else {
            &BOUNDED_INPUT_SCHEMA
        }
    }

    fn parse_native_input(
        &self,
        input: &Value,
    ) -> Option<Result<Value, tool_api::native_schema::NativeSchemaError>> {
        Tool::parse_native_input(self, &generated_builtin_result(input)).map(|result| {
            result.and_then(|parsed| {
                serde_json::from_str(
                    &parsed
                        .to_json_string()
                        .expect("test input projection serializes"),
                )
                .map_err(|error| monitor_custom_error(vec![], &error.to_string()))
            })
        })
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<TestMonitorCallResult, ToolError> {
        let result = Tool::call(self, generated_builtin_result(&input), ctx, progress).await?;
        let data = serde_json::from_str(
            &result
                .data
                .to_json_string()
                .expect("test result projection serializes"),
        )
        .expect("test result JSON parses");
        let model_content = result
            .model_content
            .as_ref()
            .and_then(|value| value.string_at(""))
            .map(str::to_owned);
        Ok(TestMonitorCallResult {
            data,
            model_content,
            is_error: result.is_error,
        })
    }
}

fn invalid_tool_input(message: impl Into<String>) -> ToolError {
    let message = message.into();
    ToolError::InvalidInput(tool_api::tool_trait::source_text_projection(&message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::task_registry::{
        TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
        TaskRegistryHandle, TaskUpdatePatch,
    };
    use mobile_linux_api::ProcessOutput;
    use std::sync::{Arc, Mutex};
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn empty_input() -> Utf16JsonProjection {
        Utf16JsonProjection::parse("{}").expect("empty object is valid JSON")
    }

    #[derive(Default)]
    struct RecordingRegistry {
        websocket: Mutex<Option<WebSocketMonitorRegistration>>,
        monitor: Mutex<Option<MonitorRegistration>>,
    }

    #[async_trait]
    impl TaskRegistryHandle for RecordingRegistry {
        async fn create(&self, _input: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used by MonitorTool")
        }

        async fn get(&self, _id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
            Ok(None)
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
            unreachable!("not used by MonitorTool")
        }

        async fn set_status(
            &self,
            _id: &str,
            _status: &str,
        ) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used by MonitorTool")
        }

        async fn kill(&self, _id: &str) -> Result<TaskRecord, TaskRegistryError> {
            unreachable!("not used by MonitorTool")
        }

        async fn spawn_websocket_monitor(
            &self,
            reg: WebSocketMonitorRegistration,
            _: Arc<dyn lingxi_core::host::HttpTransport>,
        ) -> Result<String, TaskRegistryError> {
            *self.websocket.lock().unwrap() = Some(reg);
            Ok("s12345678".into())
        }
        async fn spawn_monitor(
            &self,
            reg: MonitorRegistration,
        ) -> Result<String, TaskRegistryError> {
            *self.monitor.lock().expect("monitor lock") = Some(reg);
            Ok("m12345678".to_string())
        }

        async fn output(
            &self,
            _id: &str,
            _offset: Option<u64>,
        ) -> Result<TaskOutputChunk, TaskRegistryError> {
            unreachable!("not used by MonitorTool")
        }
    }

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
        telemetry::test_clear_flag(AMBER_SENTINEL_FLAG);
        telemetry::test_set_flag("tengu_breezy_crescent", false);
        lingxi_core::host::session_flags::set_single_shot_print_session(false);
        telemetry::test_clear_flag("tengu_kairos_push_notifications");
        lingxi_core::host::session_flags::set_agent_push_notif_enabled(false);
        g
    }
    fn tool() -> MonitorTool {
        MonitorTool::new(shell_test_ctx(dummy_out()))
    }

    fn tool_with_registry(registry: Arc<RecordingRegistry>) -> MonitorTool {
        let mut ctx = shell_test_ctx(dummy_out());
        ctx.task_registry = Some(registry);
        MonitorTool::new(ctx)
    }

    /// claude-code refuses to start a monitor whose working directory is gone
    /// (the shared shell path's `preSpawnError`), and recovers silently when
    /// the workspace is still there. Without the check the port minted a task
    /// for a directory that does not exist and only failed later, as a dead row.
    #[tokio::test]
    async fn a_missing_working_directory_is_refused_before_a_task_is_minted() {
        let _g = guard();
        telemetry::test_set_flag(AMBER_SENTINEL_FLAG, true);
        let registry = Arc::new(RecordingRegistry::default());
        let t = tool_with_registry(registry.clone());

        // The workspace exists, so a deleted per-call cwd recovers SILENTLY —
        // upstream returns no error when the first fallback works.
        let mut ctx = fresh_ctx();
        ctx.cwd = Some(std::path::PathBuf::from("/definitely/not/here/monitor"));
        let recovered = t
            .call(
                json!({"command": "tail -f log", "description": "d"}),
                ctx,
                fresh_tx(),
            )
            .await;
        assert!(
            recovered.is_ok(),
            "a recoverable cwd must not refuse: {recovered:?}"
        );
        assert!(
            registry.monitor.lock().expect("monitor lock").is_some(),
            "and it still mints the monitor",
        );
        // The recovered cwd is what the monitor records, not the dead one.
        let recorded = registry
            .monitor
            .lock()
            .expect("monitor lock")
            .as_ref()
            .and_then(|m| m.cwd.clone())
            .expect("cwd recorded");
        assert!(
            !recorded.contains("not/here/monitor"),
            "the dead cwd must not be recorded, got: {recorded}"
        );
        *registry.monitor.lock().expect("monitor lock") = None;

        // With the workspace gone too there is nothing to fall back to.
        let mut broken = shell_test_ctx(dummy_out());
        broken.session_cwd = tool_api::session_cwd::SessionCwd::new(
            std::path::PathBuf::from("/definitely/not/here/workspace"),
            Vec::new(),
        );
        broken.task_registry = Some(registry.clone());
        let t = MonitorTool::new(broken);
        let mut ctx = fresh_ctx();
        ctx.cwd = Some(std::path::PathBuf::from("/definitely/not/here/monitor"));
        let err = t
            .call(
                json!({"command": "tail -f log", "description": "d"}),
                ctx,
                fresh_tx(),
            )
            .await
            .expect_err("a monitor with no usable cwd must be refused");
        let message = format!("{err}");
        assert!(
            message.contains("no longer exists. Please restart Claude from an existing directory."),
            "byte-exact upstream copy, got: {message}"
        );
        // The telemetry label must NOT reach the model.
        assert!(
            !message.contains("cwd/argv redacted"),
            "that string is a telemetry label, not model-facing copy"
        );
        assert!(
            registry.monitor.lock().expect("monitor lock").is_none(),
            "and no task was minted for the refused call",
        );
        telemetry::test_clear_flag(AMBER_SENTINEL_FLAG);
    }

    /// claude-code `Sbn()` splices two spots on `Dl()`. With background tasks
    /// ON the description must be byte-identical to what it always was; with
    /// them OFF both spots must point at a foreground Bash loop instead, or the
    /// tool tells the model to use a parameter that has been removed from the
    /// Bash schema.
    #[tokio::test]
    async fn the_description_follows_the_background_tasks_setting() {
        let _g = guard();
        let t = tool();

        std::env::remove_var("LINGXI_DISABLE_BACKGROUND_TASKS");
        let enabled = t
            .description(
                &empty_input(),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert!(enabled.contains(
            "use **Bash with `run_in_background`** and a command that exits when the condition is true"
        ));
        assert!(enabled.contains(
            "use Bash `run_in_background` with an `until` loop instead (one notification, ends in seconds)"
        ));
        assert!(!enabled.contains("foreground with Bash"));

        std::env::set_var("LINGXI_DISABLE_BACKGROUND_TASKS", "1");
        let disabled = t
            .description(
                &empty_input(),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await;
        assert!(disabled.contains(
            "run the command in the **foreground with Bash**, exiting when the condition is true"
        ));
        assert!(disabled.contains("use a foreground Bash `until` loop instead"));
        assert!(
            !disabled.contains("run_in_background"),
            "with the parameter gone, nothing may still recommend it"
        );
        std::env::remove_var("LINGXI_DISABLE_BACKGROUND_TASKS");

        // Everything outside the two spliced spots is the same text.
        assert!(enabled.starts_with("Start a background monitor that streams events"));
        assert!(disabled.starts_with("Start a background monitor that streams events"));
    }

    #[test]
    fn name_and_gate() {
        let _g = guard();
        let t = tool();
        assert_eq!(t.name(), "Monitor");
        assert!(t.should_defer());
        assert_eq!(t.max_result_size_chars(), 10_000);
        assert!(!t.is_read_only(&empty_input()));
        // Default: amber flag off → disabled.
        assert!(!t.is_enabled(&ToolStaticContext::default()));
        telemetry::test_set_flag(AMBER_SENTINEL_FLAG, true);
        assert!(t.is_enabled(&ToolStaticContext::default()));
        telemetry::test_clear_flag(AMBER_SENTINEL_FLAG);
    }

    #[test]
    fn description_starts_with_cjr_no_push_splice_by_default() {
        let _g = guard();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let d = rt.block_on(tool().description(
            &empty_input(),
            &DescriptionOptions {
                is_non_interactive_session: false,
            },
        ));
        assert!(d.starts_with("Start a background monitor that streams events"));
        assert!(d.contains("Use TaskStop to cancel early."));
        assert!(d.contains("**ws source**"));
        assert!(d.trim_end().ends_with("Use bash when you need to transform or filter frames with shell tools before they become events."));
        // Yke() off by default → no lJr splice.
        assert!(!d.contains("send a PushNotification"));
    }

    #[test]
    fn ljr_splice_appears_when_yke_on() {
        let _g = guard();
        // Yke() = push flag && agentPushNotifEnabled.
        telemetry::test_set_flag("tengu_kairos_push_notifications", true);
        assert!(!cron::is_push_notif_enabled(), "Yke needs the setting too");
        lingxi_core::host::session_flags::set_agent_push_notif_enabled(true);
        assert!(cron::is_push_notif_enabled());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let d = rt.block_on(tool().description(
            &empty_input(),
            &DescriptionOptions {
                is_non_interactive_session: false,
            },
        ));
        // Byte-exact, INCLUDING the two leading newlines: `ybn()` returns
        // `\n\nWhen an event lands…` so the addendum opens its own paragraph.
        // The previous form of this assertion compared a local literal against
        // itself and so could not have caught the one-newline port.
        let splice = "\n\nWhen an event lands that the user would want to act on now \u{2014} an error appeared, the status they were waiting on flipped \u{2014} send a PushNotification. Not every event is worth a push; the ones that change what they'd do next are.";
        assert!(
            d.contains(splice),
            "description must carry the byte-exact ybn() splice; tail was: {:?}",
            &d[d.len().saturating_sub(400)..]
        );
        lingxi_core::host::session_flags::set_agent_push_notif_enabled(false);
        telemetry::test_clear_flag("tengu_kairos_push_notifications");
    }

    /// MON-09: a monitor command runs under the SAME sandbox decision as the
    /// identical command typed into Bash. The port used to bypass the sandbox
    /// outright, so a monitor had strictly more reach than Bash for the same
    /// text.
    #[tokio::test]
    async fn a_monitor_command_is_wrapped_by_the_shared_sandbox_decision() {
        let _g = guard();
        let registry = Arc::new(RecordingRegistry::default());
        let monitor_cwd =
            std::env::temp_dir().join(format!("lingxi-monitor-sbx-{}", std::process::id()));
        std::fs::create_dir_all(&monitor_cwd).expect("create the monitor cwd");

        let spawn_command_for = |available: bool| {
            let registry = registry.clone();
            let monitor_cwd = monitor_cwd.clone();
            async move {
                let mut ctx = shell_test_ctx(dummy_out());
                ctx.task_registry = Some(registry.clone());
                ctx.sandbox_available = available;
                ctx.sandbox_runtime.enabled = available;
                let mut call_ctx = fresh_ctx();
                call_ctx.cwd = Some(monitor_cwd);
                MonitorTool::new(ctx)
                    .call(
                        json!({ "command": "tail -f app.log", "description": "app log" }),
                        call_ctx,
                        fresh_tx(),
                    )
                    .await
                    .expect("monitor should start");
                let reg = registry
                    .monitor
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("the registration");
                (reg.command, reg.spawn_command)
            }
        };

        // No sandbox on this host ⇒ `should_use_sandbox` short-circuits and the
        // raw command is spawned, exactly as Bash would.
        let (command, unconfined) = spawn_command_for(false).await;
        assert_eq!(command.string_at(""), Some("tail -f app.log"));
        assert_eq!(unconfined, None);

        // Sandbox available and enabled ⇒ the SPAWNED form is wrapped, while
        // the recorded `command` stays the raw text the model wrote (that is
        // what `/tasks` and the notifications show).
        let (command, confined) = spawn_command_for(true).await;
        assert_eq!(command.string_at(""), Some("tail -f app.log"));
        let confined = confined.expect("an available sandbox must confine the command");
        assert_ne!(confined, "tail -f app.log", "the command must be wrapped");
        assert!(
            confined.contains("tail -f app.log"),
            "the wrap carries the original command: {confined}"
        );
    }

    #[tokio::test]
    async fn call_returns_result_shape_with_ccr_cap() {
        let _g = guard();
        let registry = Arc::new(RecordingRegistry::default());
        let mut call_ctx = fresh_ctx();
        // A REAL directory: the pre-spawn guard recovers a cwd that does not
        // exist, so a fictitious path here would silently assert the wrong
        // thing.
        let monitor_cwd =
            std::env::temp_dir().join(format!("lingxi-monitor-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&monitor_cwd).expect("create the monitor cwd");
        call_ctx.cwd = Some(monitor_cwd.clone());
        call_ctx.tool_use_id = Some(lingxi_core::types::ToolUseId::new());
        call_ctx.agent_name = Some("builder".into());
        call_ctx.team_name = Some("alpha".into());
        let creator_agent_id = lingxi_core::types::AgentId::new();
        call_ctx.agent_id = Some(creator_agent_id);
        let expected_tool_use_id = call_ctx.tool_use_id.as_ref().map(ToString::to_string);
        let out = tool_with_registry(registry.clone())
            .call(
                json!({"description": "ci", "command": "tail -f log", "persistent": true}),
                call_ctx,
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["taskId"], json!("m12345678"));
        assert_eq!(out.data["persistent"], json!(true));
        assert_eq!(out.data["timeoutMs"], json!(0)); // persistent → 0
        assert!(out.model_content.as_deref().unwrap().contains("persistent"));
        let launched = registry
            .monitor
            .lock()
            .expect("monitor lock")
            .clone()
            .unwrap();
        assert_eq!(launched.command.string_at(""), Some("tail -f log"));
        assert_eq!(launched.description.string_at(""), Some("ci"));
        assert!(launched.persistent);
        assert_eq!(launched.timeout_ms, 0);
        assert_eq!(
            launched.cwd.as_deref(),
            Some(monitor_cwd.to_string_lossy().as_ref())
        );
        assert_eq!(launched.tool_use_id, expected_tool_use_id);
        assert_eq!(launched.creator_teammate_name.as_deref(), Some("builder"));
        assert_eq!(launched.creator_team_name.as_deref(), Some("alpha"));
        assert_eq!(launched.creator_agent_id, Some(creator_agent_id));
    }

    #[tokio::test]
    async fn call_fails_closed_without_registry() {
        let error = tool()
            .call(
                json!({"description": "ci", "command": "echo ready"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("missing registry must not mint a fake task id");
        assert!(error
            .to_string()
            .contains("task registry is not configured"));
    }

    #[tokio::test]
    async fn native_monitor_schema_matches_270_on_actual_nested_dispatch() {
        use lingxi_core::host::tool_invoker::{
            SubagentInvocationContext, ToolExecutionPolicy, ToolInvoker,
        };
        fn invocation_context() -> SubagentInvocationContext {
            SubagentInvocationContext {
                available_tool_names: None,
                cancellation_token: lingxi_core::host::CancellationToken::new(),
                permission_pause_observer: None,
                parent_agent_id: None,
                native_query_depth: None,
                origin_session_id: None,
                instruction_context: None,
                fork_context: None,
                tool_execution_policy: ToolExecutionPolicy::Ordinary,
                agent_name: None,
                team_name: None,
                is_async: false,
                is_non_interactive_session: true,
                can_show_permission_prompts: false,
                cwd: None,
                tool_use_id: None,
                assistant_message_id: None,
                depth: 0,
                observer: None,
                parent_model: None,
                parent_model_profile: None,
                agent_spawn_provenance: Default::default(),
                tool_context_state: None,
                assistant_message: None,
                same_turn_tool_uses: Vec::new(),
                current_history: Vec::new(),
                mode_override: None,
                request_source: None,
                frozen_command_denies: Vec::new(),
            }
        }

        let _g = guard();
        let oracle: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/monitor_schema_2_1_270.json"
        ))
        .unwrap();
        let monitor = Arc::new(tool());
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(monitor.clone());
        let invoker = tool_api::RegistryToolInvoker::new(Arc::new(registry));
        for (index, case) in oracle["cases"].as_array().unwrap().iter().enumerate() {
            let bounded = case["mode"] == "bounded";
            telemetry::test_set_flag("tengu_breezy_crescent", bounded);
            let expected_schema = oracle["schemas"][case["mode"].as_str().unwrap()].clone();
            assert_eq!(*monitor.input_schema(), expected_schema, "schema {index}");
            let parsed = monitor.parse_native_input(&case["input"]).unwrap();
            if case["success"] == true {
                assert_eq!(parsed.unwrap(), case["normalized"], "parsed {index}");
            } else {
                let error = parsed.unwrap_err();
                assert_eq!(error.raw.string_at(""), case["raw"].as_str(), "raw {index}");
                assert_eq!(
                    error.display.string_at(""),
                    case["display"].as_str(),
                    "display {index}"
                );
                let error = invoker
                    .invoke(
                        "Monitor",
                        generated_builtin_result(&case["input"]),
                        invocation_context(),
                    )
                    .await
                    .unwrap_err();
                let expected = format!(
                    "<tool_use_error>InputValidationError: {}</tool_use_error>",
                    case["display"].as_str().unwrap()
                );
                assert_eq!(
                    error.model_tool_result_content().string_at(""),
                    Some(expected.as_str()),
                    "nested {index}"
                );
            }
        }
        telemetry::test_set_flag("tengu_breezy_crescent", false);
        let tasks = Arc::new(RecordingRegistry::default());
        let tool = tool_with_registry(tasks.clone());
        let input =
            json!({"description":"fractional", "command":"echo ready", "timeout_ms":1000.5});
        let parsed = tool.parse_native_input(&input).unwrap().unwrap();
        let result = tool.call(parsed, fresh_ctx(), fresh_tx()).await.unwrap();
        assert_eq!(result.data["timeoutMs"], json!(1000.5));
        assert_eq!(
            result.model_content.as_deref(),
            oracle["fractionalResult"].as_str()
        );
        assert_eq!(
            tasks.monitor.lock().unwrap().as_ref().unwrap().timeout_ms,
            1000
        );
    }

    #[test]
    fn monitor_schema_preserves_unpaired_command_and_description_units() {
        let input =
            Utf16JsonProjection::parse(r#"{"description":"note \udfff","command":"\ud800echo"}"#)
                .expect("input with escaped unpaired units is valid JSON");
        let parsed = parse_monitor_input(&input, false).expect("JS strings satisfy string schema");

        assert_eq!(
            parsed.string_units("/description"),
            Some(vec![
                u16::from(b'n'),
                u16::from(b'o'),
                u16::from(b't'),
                u16::from(b'e'),
                u16::from(b' '),
                0xDFFF,
            ])
        );
        assert_eq!(
            parsed.string_units("/command"),
            Some(vec![
                0xD800,
                u16::from(b'e'),
                u16::from(b'c'),
                u16::from(b'h'),
                u16::from(b'o')
            ])
        );
        assert_eq!(
            text_encoder_string(&parsed.subprojection("/command").unwrap()),
            "\u{FFFD}echo"
        );
    }

    #[test]
    fn schema_rejects_control_chars_and_huge_timeout() {
        let _g = guard();
        let t = tool();
        for input in [
            json!({"description":"d","command":"echo \u{7}hi"}),
            json!({"description":"d","command":"echo ok","timeout_ms":9000000}),
        ] {
            assert!(t.parse_native_input(&input).unwrap().is_err());
        }
        assert!(t.parse_native_input(&json!({"description":"d","command":"echo ok","timeout_ms":9000000,"persistent":true})).unwrap().is_ok());
        assert!(t
            .parse_native_input(&json!({"description":"d","command":"tail -f x"}))
            .unwrap()
            .is_ok());
    }

    #[tokio::test]
    async fn websocket_schema_validation_permission_and_dispatch_are_connected() {
        let _g = guard();
        let registry = Arc::new(RecordingRegistry::default());
        let mut context = shell_test_ctx(dummy_out());
        context.task_registry = Some(registry.clone());
        context.http = Arc::new(PreflightHttp { allow: true });
        let t = MonitorTool::new(context);
        let input = generated_builtin_result(
            &json!({"ws":{"url":"wss://events.example.com/feed","protocols":["v1"]},"description":"deploy","persistent":true}),
        );
        t.validate_input(&input, &fresh_ctx()).await.unwrap();
        assert!(matches!(
            t.check_permissions(&input, &fresh_ctx()).await,
            PermissionResult::Ask { .. }
        ));
        let result = t
            .call(
                serde_json::from_str(&input.to_json_string().unwrap()).unwrap(),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data["taskId"].as_str(), Some("s12345678"));
        let reg = registry.websocket.lock().unwrap().clone().unwrap();
        assert_eq!(reg.url, "wss://events.example.com/feed");
        assert_eq!(reg.protocols, vec!["v1"]);
        assert!(reg.task.persistent);
        assert_eq!(reg.task.timeout_ms, 0);
        assert!(
            registry.monitor.lock().unwrap().is_none(),
            "a socket never spawns Bash"
        );
        for bad in [
            json!({"description":"x"}),
            json!({"description":"x","command":"echo hi","ws":{"url":"wss://example.com"}}),
            json!({"description":"x","ws":{"url":"https://example.com"}}),
            json!({"description":"x","ws":{"url":"wss://u:p@example.com"}}),
            json!({"description":"x","ws":{"url":"wss://example.com","protocols":["v1","v1"]}}),
        ] {
            assert!(t.parse_native_input(&bad).unwrap().is_err(), "{bad}");
        }
        assert!(matches!(
            t.check_permissions(
                &generated_builtin_result(&json!({"ws":{"url":"ws://169.254.169.254"}})),
                &fresh_ctx(),
            )
            .await,
            PermissionResult::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn websocket_permission_protocol_preview_matches_270_bytes() {
        let _g = guard();
        let tool = MonitorTool::new(shell_test_ctx(dummy_out()));
        for (protocols, suffix) in [
            (json!([]), String::new()),
            (
                json!(["v1", "v2"]),
                " (subprotocols: \"v1\", \"v2\")".into(),
            ),
            (
                json!(["a".repeat(49), "b", "c", "d", "e"]),
                format!(
                    " (subprotocols: \"{}…\", \"b\", \"c\", \"d\" (+1 more))",
                    "a".repeat(48)
                ),
            ),
        ] {
            let input = generated_builtin_result(
                &json!({"ws":{"url":"wss://events.example.com/feed", "protocols":protocols}}),
            );
            let PermissionResult::Ask { prompt, .. } =
                tool.check_permissions(&input, &fresh_ctx()).await
            else {
                panic!("public WebSocket should request permission");
            };
            let expected =
                format!("Monitor will open a WebSocket to wss://events.example.com/feed{suffix}");
            assert_eq!(prompt.message.string_at(""), Some(expected.as_str()));
        }
    }
    struct PreflightHttp {
        allow: bool,
    }
    #[async_trait]
    impl lingxi_core::host::HttpTransport for PreflightHttp {
        async fn request(
            &self,
            _: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            unreachable!()
        }
        async fn stream_sse(
            &self,
            _: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            unreachable!()
        }
        async fn preflight_monitor_websocket(
            &self,
            _: &str,
        ) -> Result<(), lingxi_core::host::HttpError> {
            if self.allow {
                Ok(())
            } else {
                Err(lingxi_core::host::HttpError::InvalidRequest(
                    "DNS resolved private address".into(),
                ))
            }
        }
    }
    #[tokio::test]
    async fn websocket_dns_refusal_never_registers_a_task() {
        let registry = Arc::new(RecordingRegistry::default());
        let mut context = shell_test_ctx(dummy_out());
        context.task_registry = Some(registry.clone());
        context.http = Arc::new(PreflightHttp { allow: false });
        let tool = MonitorTool::new(context);
        let result = tool
            .call(
                json!({"ws":{"url":"wss://internal.example.com"},"description":"x"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await;
        assert!(result.is_err());
        assert!(registry.websocket.lock().unwrap().is_none());
    }
    #[test]
    fn bounded_monitors_are_enabled_by_default_in_286() {
        let _g = guard();
        telemetry::test_clear_flag("tengu_breezy_crescent");
        let monitor = tool();
        assert!(cron::bounded_monitors_enabled());
        assert_eq!(*monitor.input_schema(), *BOUNDED_INPUT_SCHEMA);
        let parsed = monitor
            .parse_native_input(&json!({
                "description": "watch", "command": "tail -f log", "persistent": true
            }))
            .unwrap()
            .unwrap();
        assert!(parsed.get("persistent").is_none());
        assert_eq!(apply_ccr_timeout_cap(3_600_000, true), (1_800_000, false));
        lingxi_core::host::session_flags::set_single_shot_print_session(true);
        assert_eq!(*monitor.input_schema(), *SINGLE_SHOT_INPUT_SCHEMA);
        assert_eq!(apply_ccr_timeout_cap(3_600_000, true), (600_000, false));
        lingxi_core::host::session_flags::set_single_shot_print_session(false);
        telemetry::test_set_flag("tengu_breezy_crescent", false);
    }

    #[tokio::test]
    async fn bounded_monitor_schema_cap_and_result_match_270() {
        let _guard = guard();
        telemetry::test_set_flag("tengu_breezy_crescent", true);
        let t = tool();
        assert!(t.input_schema()["properties"].get("persistent").is_none());
        assert_eq!(
            t.input_schema()["properties"]["timeout_ms"]["maximum"],
            3_600_000
        );
        assert_eq!(t.input_schema()["properties"]["timeout_ms"]["description"], "Kill the monitor after this deadline. Default 300000ms. Deadlines above 1800000ms are capped to 1800000ms. You are notified at expiry and can re-arm.");
        assert_eq!(apply_ccr_timeout_cap(3_600_000, true), (1_800_000, false));
        assert_eq!(started_message("b123", 1_800_000.0, false), "Monitor started (task b123, expires in 30m unless the source ends first; you get one notice at expiry — re-arm if you still need the watch). You will be notified on each event. Keep working — do not poll or sleep. Events may arrive while you are waiting for the user — an event is not their reply.");
        assert!(cjr().contains(
            "Every monitor expires after `timeout_ms` (default 5 minutes, at most 30 minutes)"
        ));
        assert!(t.parse_native_input(&json!({"description":"d","command":"echo ok","timeout_ms":9_000_000,"persistent":true})).unwrap().is_err());
        lingxi_core::host::session_flags::set_single_shot_print_session(true);
        assert_eq!(apply_ccr_timeout_cap(3_600_000, false), (600_000, false));
        assert_eq!(t.input_schema()["properties"]["timeout_ms"]["description"], "Kill the monitor after this deadline. Default 300000ms. Deadlines above 600000ms are capped to 600000ms. You are notified at expiry and can re-arm.");
        let registry = Arc::new(RecordingRegistry::default());
        let out = tool_with_registry(registry.clone()).call(
            json!({"description":"ci", "command":"tail -f log", "timeout_ms":3_600_000, "persistent":true}),
            fresh_ctx(), fresh_tx(),
        ).await.expect("bounded monitor starts");
        let launched = registry.monitor.lock().unwrap().clone().unwrap();
        assert_eq!(launched.timeout_ms, 600_000);
        assert!(!launched.persistent);
        assert_eq!(out.data["timeoutMs"], 600_000);
        assert_eq!(out.data["persistent"], false);
        assert!(out
            .model_content
            .unwrap()
            .contains("expires in 10m unless the source ends first"));
        lingxi_core::host::session_flags::set_single_shot_print_session(false);
        telemetry::test_set_flag("tengu_breezy_crescent", false);
    }
    #[test]
    fn legacy_remote_cap_uses_javascript_environment_truthiness() {
        let _guard = guard();
        assert_eq!(apply_ccr_timeout_cap(3_600_000, true), (3_600_000, true));
        std::env::set_var("LINGXI_REMOTE", "0");
        assert_eq!(apply_ccr_timeout_cap(3_600_000, true), (1_800_000, false));
        assert_eq!(apply_ccr_timeout_cap(300_000, false), (300_000, false));
        std::env::remove_var("LINGXI_REMOTE");
    }
}
