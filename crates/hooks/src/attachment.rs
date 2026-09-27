//! Per-hook-run transcript `attachment` records.
//!
//! claude-code persists exactly ONE `attachment` transcript line for every
//! hook run. The attachment payload's `type` discriminates the outcome; this
//! module builds the three run-outcome payloads the port was missing —
//! `hook_success`, `hook_non_blocking_error`, `hook_cancelled` — with
//! claude's EXACT key ORDER and conditional-field presence.
//!
//! # Oracle evidence
//!
//! Key ORDER is byte-locked against BOTH sources:
//!
//! * **Real 2.1.220 transcripts** (`~/.claude/projects/**/*.jsonl`, 26 048
//!   hook attachment records mined): every one of the 25 901 `hook_success`
//!   records has the key tuple
//!   `(type, hookName, toolUseID, hookEvent, content, stdout, stderr,
//!   exitCode, command, durationMs)`; `hook_non_blocking_error` has
//!   `(type, hookName, toolUseID, hookEvent, stderr, stdout, exitCode,
//!   command, durationMs)`; `hook_cancelled` has
//!   `(type, hookName, toolUseID, hookEvent, command, durationMs, timedOut,
//!   timeoutMs)`.
//! * **The 2.1.220 binary** (`~/.local/share/claude/versions/2.1.220`), the
//!   command-hook runner at BIN off **237798900–237806040**:
//!   - exit-0 success — `Va({type:"hook_success",hookName:f,toolUseID:r,
//!     hookEvent:p,content:Ae,stdout:Ce.stdout,stderr:Ce.stderr,
//!     exitCode:Ce.status,command:ee,durationMs:Ee})`
//!   - non-blocking error — `Va({type:"hook_non_blocking_error",hookName:f,
//!     toolUseID:r,hookEvent:p,stderr:…,stdout:…,exitCode:…,command:ee,
//!     durationMs:Ee})`
//!   - aborted — `Va({type:"hook_cancelled",hookName:f,toolUseID:r,
//!     hookEvent:p,command:ee,durationMs:Ee,timedOut:!o?.aborted,
//!     timeoutMs:re})`
//!
//! `command` is `ee = qq(hook)` (BIN off **230279724**): the hook's
//! `statusMessage` when set, else `iSe(hook)` (BIN off **230279432**) — the
//! per-arm rendering `command [args…]` / `prompt` / `url` /
//! `server/tool` / `"callback"` / `"function"`.
//!
//! Conditional presence (keys must be ABSENT, never `null`):
//! * `command` / `durationMs` on `hook_non_blocking_error` are omitted by the
//!   HTTP and `mcp_tool` arms (BIN off 237799357 / 237800962), which yield the
//!   payload without them.
//! * `timedOut` / `timeoutMs` on `hook_cancelled` travel as a PAIR: the
//!   command / HTTP / `mcp_tool` cancel arms emit both, the prompt / agent
//!   early-return arm (BIN off 237798423) and the `PostToolUse` re-yield
//!   (BIN off 234725805) emit NEITHER — hence [`CancellationTimeout`] models
//!   them as one optional unit rather than two independent options.

use crate::definition::{HookDefinition, HookExecutor};
use crate::events::HookEvent;
use async_trait::async_trait;
use serde_json::{Map, Value};

/// Sink the executor publishes each hook-run attachment to.
///
/// The engine wires an implementation that appends one `type:"attachment"`
/// line to the session transcript. Without a sink, synchronous attachments are
/// still carried on [`crate::AggregateHookResult::hook_attachments`].
/// Detached completions cannot be added to an aggregate that has already
/// returned, so they are retained only when this sink is wired.
#[async_trait]
pub trait HookAttachmentSink: Send + Sync {
    /// Persist one hook-run attachment payload.
    async fn record(&self, attachment: Value);

    /// Persist oversized hook output and return the transcript-visible path
    /// reference. Hosts without session storage may return `None`; callers then
    /// retain the legacy inline truncation fallback.
    async fn persist_large_output(&self, text: &str) -> Option<String> {
        let _ = text;
        None
    }
}

/// claude's inline cap for hook output spliced into an attachment's `content`
/// (`P0u = 1e4`, BIN off **230268805**; `jKe(e,t,r,n=P0u)` returns `e`
/// unchanged when `e.length <= n`).
pub const HOOK_OUTPUT_INLINE_LIMIT: usize = 10_000;

/// Apply claude's `jKe` inline-vs-persist threshold to hook output.
///
/// At or under [`HOOK_OUTPUT_INLINE_LIMIT`] chars the text is returned
/// verbatim — the byte-exact common case. The production attachment sink
/// persists larger output before this fallback is used; hosts without session
/// storage still truncate safely at the same boundary.
#[must_use]
pub fn inline_hook_output(text: &str) -> String {
    if text.chars().count() <= HOOK_OUTPUT_INLINE_LIMIT {
        return text.to_string();
    }
    text.chars().take(HOOK_OUTPUT_INLINE_LIMIT).collect()
}

/// The four identity fields every hook-run attachment leads with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookAttachmentIdentity {
    /// claude's `hookName` — see [`hook_name_for_event`].
    pub hook_name: String,
    /// claude's `hookEvent` — the bare event name (`"PostToolUse"`, `"Stop"`).
    pub hook_event: String,
    /// claude's `toolUseID` — the provider tool-use id for tool events, else a
    /// freshly minted uuid (see [`tool_use_id_for_event`]).
    pub tool_use_id: String,
}

/// The `timedOut` / `timeoutMs` pair carried by a `hook_cancelled` attachment.
///
/// Modeled as one unit because claude emits both keys or neither — never one
/// (BIN off 237798900: `timedOut:!o?.aborted,timeoutMs:re`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancellationTimeout {
    /// `!outerSignal?.aborted` — `true` when the abort came from the hook's own
    /// execution timeout, `false` when the caller (user Esc / turn teardown)
    /// aborted it. NOTE: a caller abort still emits the pair with `false`; it
    /// is the prompt/agent early-return arm that omits both keys.
    pub timed_out: bool,
    /// The deadline that was in force — `hook.timeout * 1000` when the hook
    /// declares one, else the runner's default (`re=q.timeout?q.timeout*1000:i`).
    pub timeout_ms: u64,
}

/// Seed the four leading identity keys in claude's order.
fn identity_head(kind: &str, id: &HookAttachmentIdentity) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("type".into(), Value::String(kind.into()));
    m.insert("hookName".into(), Value::String(id.hook_name.clone()));
    m.insert("toolUseID".into(), Value::String(id.tool_use_id.clone()));
    m.insert("hookEvent".into(), Value::String(id.hook_event.clone()));
    m
}

/// Build a `hook_additional_context` attachment payload.
///
/// Key order is `type, content, hookName, toolUseID, hookEvent` — DIFFERENT
/// from every hook-RUN attachment (which leads with `hookName`), so this must
/// NOT route through [`identity_head`]. `content` is an ARRAY of strings.
///
/// # Oracle evidence
///
/// Identical at all 13 producer sites in the 2.1.220 binary and on all 145
/// real `type:"attachment"` records with `attachment.type ==
/// "hook_additional_context"`:
///
/// | site | BIN off | `hookName` | `toolUseID` | `hookEvent` |
/// |---|---|---|---|---|
/// | SessionStart | 232675554 | `"SessionStart"` (BARE) | `"SessionStart"` (literal) | `"SessionStart"` |
/// | Setup | 232676612 | `"Setup"` | `"Setup"` | `"Setup"` |
/// | Stop | 233240830 | `"Stop"` | `` `hook-${randomUUID()}` `` | `"Stop"` |
/// | UserPromptSubmit | 233257781 | `"UserPromptSubmit"` | `` `hook-${uuid}` `` | `"UserPromptSubmit"` |
/// | UserPromptExpansion | 233262039 | `"UserPromptExpansion"` | `` `hook-${uuid}` `` | `"UserPromptExpansion"` |
/// | SubagentStart | 233273539 | `"SubagentStart"` | `randomUUID()` (no prefix) | `"SubagentStart"` |
/// | PostToolBatch | 233160690 | `"PostToolBatch"` | tool-batch id | `"PostToolBatch"` |
/// | PreToolUse | 234733097 | `` `PreToolUse:${tool}` `` | the `toolu_…` id | `"PreToolUse"` |
/// | PostToolUse | 234726655 | `` `PostToolUse:${tool}` `` | the `toolu_…` id | `"PostToolUse"` |
/// | PostToolUseFailure | 234728470 | `` `PostToolUseFailure:${tool}` `` | the `toolu_…` id | `"PostToolUseFailure"` |
///
/// NOTE the SessionStart row: the identity is a pair of LITERALS, NOT
/// [`hook_name_for_event`]'s `SessionStart:{source}` and NOT a minted uuid —
/// hence the plain `&str` parameters here rather than a
/// [`HookAttachmentIdentity`].
///
/// The renderer (BIN off 238107100) turns this attachment into an EPHEMERAL
/// `isMeta` user message for the model
/// (`` <system-reminder>\n${hookName} hook additional context:
/// ${content.join("\n")}\n</system-reminder> ``); that message is never
/// persisted — this attachment line IS the on-disk record. Confirmed by
/// census: 145 attachment lines, 0 persisted `user` lines carrying the
/// rendered text.
#[must_use]
pub fn additional_context_attachment(
    hook_name: &str,
    tool_use_id: &str,
    hook_event: &str,
    content: &[String],
) -> Value {
    let mut m = Map::new();
    m.insert(
        "type".into(),
        Value::String("hook_additional_context".into()),
    );
    m.insert(
        "content".into(),
        Value::Array(content.iter().cloned().map(Value::String).collect()),
    );
    m.insert("hookName".into(), Value::String(hook_name.into()));
    m.insert("toolUseID".into(), Value::String(tool_use_id.into()));
    m.insert("hookEvent".into(), Value::String(hook_event.into()));
    Value::Object(m)
}

/// Build a `hook_error_during_execution` attachment payload (CONSUMER arm).
///
/// Key order `type, content, hookName, toolUseID, hookEvent`; `content` is a
/// STRING (unlike [`additional_context_attachment`]'s array).
///
/// # Oracle evidence
///
/// Binary ONLY — there are ZERO `hook_error_during_execution` records in the
/// mined real 2.1.220 transcripts, so this order rests on the four consumer /
/// catch sites:
/// * BIN off **234727060** — `PostToolUse:${tool}` catch
/// * BIN off **234728891** — `PostToolUseFailure:${tool}` catch
/// * BIN off **234733866** — `PreToolUse:${tool}` catch
/// * BIN off **235421957** — the `updatedToolOutput` schema-mismatch notice
///
/// The binary ALSO has three RUNNER sites (BIN off 237796991 / 237797427 /
/// 237821146) with a DIFFERENT order — `type, hookName, toolUseID, hookEvent,
/// content[, command, durationMs]`. The port has no producer for those arms,
/// so that variant is deliberately NOT modeled (a dead constructor would be
/// unverifiable).
///
/// The renderer maps this attachment type to `[]` (BIN off 238107100:
/// `hook_error_during_execution: () => []`) — the MODEL NEVER SEES IT. It is a
/// transcript + TUI-warning record only.
#[must_use]
pub fn error_during_execution_attachment(
    content: &str,
    hook_name: &str,
    tool_use_id: &str,
    hook_event: &str,
) -> Value {
    let mut m = Map::new();
    m.insert(
        "type".into(),
        Value::String("hook_error_during_execution".into()),
    );
    m.insert("content".into(), Value::String(content.into()));
    m.insert("hookName".into(), Value::String(hook_name.into()));
    m.insert("toolUseID".into(), Value::String(tool_use_id.into()));
    m.insert("hookEvent".into(), Value::String(hook_event.into()));
    Value::Object(m)
}

/// Build a `hook_success` attachment payload.
///
/// Key order `type, hookName, toolUseID, hookEvent, content, stdout, stderr,
/// exitCode, command, durationMs` (25 901 real 2.1.220 transcript records;
/// BIN off 237803277).
#[must_use]
pub fn success_attachment(
    id: &HookAttachmentIdentity,
    content: &str,
    stdout: &str,
    stderr: &str,
    exit_code: i32,
    command: &str,
    duration_ms: u64,
) -> Value {
    let mut m = identity_head("hook_success", id);
    m.insert("content".into(), Value::String(content.into()));
    m.insert("stdout".into(), Value::String(stdout.into()));
    m.insert("stderr".into(), Value::String(stderr.into()));
    m.insert("exitCode".into(), Value::from(exit_code));
    m.insert("command".into(), Value::String(command.into()));
    m.insert("durationMs".into(), Value::from(duration_ms));
    Value::Object(m)
}

/// Build a `hook_non_blocking_error` attachment payload.
///
/// Key order `type, hookName, toolUseID, hookEvent, stderr, stdout, exitCode[,
/// command, durationMs]`. `command` / `duration_ms` are `None` for the HTTP and
/// `mcp_tool` arms, which omit both keys entirely (BIN off 237799357).
#[must_use]
pub fn non_blocking_error_attachment(
    id: &HookAttachmentIdentity,
    stderr: &str,
    stdout: &str,
    exit_code: i32,
    command: Option<&str>,
    duration_ms: Option<u64>,
) -> Value {
    let mut m = identity_head("hook_non_blocking_error", id);
    m.insert("stderr".into(), Value::String(stderr.into()));
    m.insert("stdout".into(), Value::String(stdout.into()));
    m.insert("exitCode".into(), Value::from(exit_code));
    if let Some(c) = command {
        m.insert("command".into(), Value::String(c.into()));
    }
    if let Some(d) = duration_ms {
        m.insert("durationMs".into(), Value::from(d));
    }
    Value::Object(m)
}

/// Build a `hook_cancelled` attachment payload.
///
/// Key order `type, hookName, toolUseID, hookEvent[, command, durationMs][,
/// timedOut, timeoutMs]`. All four trailing keys are omitted by the
/// prompt / agent early-return arm (BIN off 237798423).
#[must_use]
pub fn cancelled_attachment(
    id: &HookAttachmentIdentity,
    command: Option<&str>,
    duration_ms: Option<u64>,
    timeout: Option<CancellationTimeout>,
) -> Value {
    let mut m = identity_head("hook_cancelled", id);
    if let Some(c) = command {
        m.insert("command".into(), Value::String(c.into()));
    }
    if let Some(d) = duration_ms {
        m.insert("durationMs".into(), Value::from(d));
    }
    if let Some(t) = timeout {
        m.insert("timedOut".into(), Value::Bool(t.timed_out));
        m.insert("timeoutMs".into(), Value::from(t.timeout_ms));
    }
    Value::Object(m)
}

/// The nested payload of a `hook_blocking_error` attachment.
///
/// Serializes as `{"blockingError":…,"command":…}` — that inner key order is
/// fixed by the sole construction site (BIN off **237775430**):
/// `u.blockingError={blockingError:…,command:t}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingError {
    /// The rendered failure text — `` `[${command}]: ${stderr || "No stderr
    /// output"}` `` on the exit-2 arm, or the hook's own `reason` on a
    /// `permissionDecision:"deny"` / elicitation `decline`.
    pub blocking_error: String,
    /// `qq(hook)` — the same rendering [`attachment_command`] produces.
    pub command: String,
}

/// Build a `hook_blocking_error` attachment payload.
///
/// Key order `type, hookName, toolUseID, hookEvent, blockingError` — the only
/// O2 payload compatible with [`identity_head`].
///
/// # Oracle evidence
///
/// **Binary only — 0 records in the mined real 2.1.220 transcripts.** Three
/// agreeing construction sites:
/// * BIN off **237778531** — `Tfn`, the JSON-stdout parser's executor-internal
///   yield. EVERY caller filters this one out before re-emitting, so it never
///   reaches a transcript on its own.
/// * BIN off **234726074** — the `PostToolUse` consumer's re-emit, with
///   `hookName: `PostToolUse:${tool}``. This is the arm that actually persists.
/// * BIN off **234728254** — the `PostToolUseFailure` twin.
///
/// NOTE the exit-2 arm of the plain-text runner (BIN off **237805098**) yields
/// a BARE `{blockingError, outcome:"blocking"}` signal with NO `message`, i.e.
/// no attachment — the CALLER builds this record. That is why
/// `build_run_attachment` returns `None` for a `Block` decision; do not "fix"
/// that guard by publishing here.
///
/// The model-facing rendering is [`blocking_error_prose`] (BIN off 238107476).
#[must_use]
pub fn blocking_error_attachment(id: &HookAttachmentIdentity, err: &BlockingError) -> Value {
    let mut m = identity_head("hook_blocking_error", id);
    let mut inner = Map::new();
    inner.insert(
        "blockingError".into(),
        Value::String(err.blocking_error.clone()),
    );
    inner.insert("command".into(), Value::String(err.command.clone()));
    m.insert("blockingError".into(), Value::Object(inner));
    Value::Object(m)
}

/// The model-facing rendering of a `hook_blocking_error` attachment.
///
/// BIN off **238107476**: `` `${e.hookName} hook blocking error from command:
/// "${e.blockingError.command}": ${e.blockingError.blockingError}` ``, wrapped
/// by the caller in a `<system-reminder>` and delivered as an `isMeta` user
/// message. Unlike most hook attachments this one IS model-facing.
#[must_use]
pub fn blocking_error_prose(hook_name: &str, err: &BlockingError) -> String {
    format!(
        "{hook_name} hook blocking error from command: \"{}\": {}",
        err.command, err.blocking_error
    )
}

/// Build a `hook_stopped_continuation` attachment payload.
///
/// Key order `type, message, hookName, toolUseID, hookEvent` — `message` sits
/// SECOND, so this must NOT route through [`identity_head`].
///
/// # Oracle evidence
///
/// **Binary only — 0 records in the mined real 2.1.220 transcripts.** Six
/// construction sites all agree on the order:
///
/// | site | BIN off | `hookName` | default `message` |
/// |---|---|---|---|
/// | Stop | 233101239 | `"Stop"` | `"Stop hook prevented continuation"` |
/// | TaskCompleted | 233102581 | `"TaskCompleted"` | `"TaskCompleted hook prevented continuation"` |
/// | TeammateIdle | 233103155 | `"TeammateIdle"` | `"TeammateIdle hook prevented continuation"` |
/// | PostToolBatch | 233161369 | `"PostToolBatch"` | `"Execution stopped by PostToolBatch hook"` |
/// | PostToolUse | 234726408 | `` `PostToolUse:${tool}` `` | `"Execution stopped by PostToolUse hook"` |
/// | PreToolUse | 235403061 | `` `PreToolUse:${tool}` `` | `"Execution stopped by hook"` |
///
/// The model-facing rendering is [`stopped_continuation_prose`]
/// (BIN off 238107808).
#[must_use]
pub fn stopped_continuation_attachment(id: &HookAttachmentIdentity, message: &str) -> Value {
    let mut m = Map::new();
    m.insert(
        "type".into(),
        Value::String("hook_stopped_continuation".into()),
    );
    m.insert("message".into(), Value::String(message.into()));
    m.insert("hookName".into(), Value::String(id.hook_name.clone()));
    m.insert("toolUseID".into(), Value::String(id.tool_use_id.clone()));
    m.insert("hookEvent".into(), Value::String(id.hook_event.clone()));
    Value::Object(m)
}

/// Build a `hook_system_message` attachment payload.
///
/// Key order `type, content, hookName, toolUseID, hookEvent` — `content` sits
/// SECOND (a STRING, unlike `hook_additional_context`'s array), so this must
/// NOT route through [`identity_head`]. `content` is passed through
/// [`inline_hook_output`] (`jKe`, the 10 000-char cap) exactly as the oracle
/// does before building the payload.
///
/// # Oracle evidence
///
/// **Binary only — 0 records in the mined real 2.1.220 transcripts.** Two
/// independent, agreeing construction sites inside the hook runner `uL`:
/// * BIN off **237807875** — the main per-result loop. Emitted AFTER that
///   iteration's run-outcome attachment, so the transcript order is
///   `hook_success` then `hook_system_message`.
/// * BIN off **237794905** — the callback-hook loop.
///
/// The renderer maps this type to `[]` (BIN off 238109329:
/// `hook_system_message:()=>[]`) — the MODEL NEVER SEES IT. It is a transcript
/// + TUI record only, which is exactly what
/// [`crate::AggregateHookResult::system_messages`]'s doc comment already says.
#[must_use]
pub fn system_message_attachment(id: &HookAttachmentIdentity, content: &str) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("hook_system_message".into()));
    m.insert("content".into(), Value::String(inline_hook_output(content)));
    m.insert("hookName".into(), Value::String(id.hook_name.clone()));
    m.insert("toolUseID".into(), Value::String(id.tool_use_id.clone()));
    m.insert("hookEvent".into(), Value::String(id.hook_event.clone()));
    Value::Object(m)
}

/// Build a `hook_deferred_tool` attachment payload.
///
/// Key order `type, toolUseID, toolName, toolInput, hookName, hookEvent,
/// permissionMode[, traceparent]` — NO identity block at all (`toolUseID` leads and
/// `hookName` sits sixth), hence the plain parameters rather than a
/// [`HookAttachmentIdentity`].
///
/// # Oracle evidence
///
/// **Binary only — 0 records in the mined real 2.1.220 transcripts**, and only
/// ONE construction site: BIN off **235409134**, the pre-tool driver's
/// `case"defer"` arm.
///
/// This record is FUNCTIONAL, not cosmetic — it is the resume protocol:
/// * `QAs` (BIN off **237925753**) reads the last 1 MiB of the transcript,
///   scans backwards for `'"hook_deferred_tool"'`, requires
///   `type==="attachment" && attachment?.type==="hook_deferred_tool"`, and
///   rejects the deferral if a LATER line carries
///   `"tool_use_id":"<that toolUseID>"`.
/// * The stream-json engine (BIN off **240899919**) turns it back into
///   `{id: toolUseID, name: toolName, input: toolInput}` with
///   `stop_reason = "tool_deferred"`.
///
/// So it must be PERSISTED, never sent to the model: the renderer is
/// `hook_deferred_tool:()=>[]` (BIN off 238109388).
///
/// `tool_input` is the HOOK-UPDATED input (after `hookUpdatedInput` and
/// `backfillObservableInput`), not the raw model input.
///
/// When OpenTelemetry is active claude appends the current `traceparent` as
/// the final key via `...re&&{traceparent:re}` at the sole producer site.
#[must_use]
pub fn deferred_tool_attachment(
    tool_use_id: &str,
    tool_name: &str,
    tool_input: &Value,
    hook_name: &str,
    permission_mode: &str,
    traceparent: Option<&str>,
) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String("hook_deferred_tool".into()));
    m.insert("toolUseID".into(), Value::String(tool_use_id.into()));
    m.insert("toolName".into(), Value::String(tool_name.into()));
    m.insert("toolInput".into(), tool_input.clone());
    m.insert("hookName".into(), Value::String(hook_name.into()));
    m.insert("hookEvent".into(), Value::String("PreToolUse".into()));
    m.insert(
        "permissionMode".into(),
        Value::String(permission_mode.into()),
    );
    if let Some(traceparent) = traceparent {
        m.insert("traceparent".into(), Value::String(traceparent.into()));
    }
    Value::Object(m)
}

/// claude's `hookName` for an event.
///
/// Tool-scoped events qualify the event name with the tool
/// (`` `PostToolUse:${t.name}` ``, BIN off 234725805) and `SessionStart`
/// qualifies with its source (`` `SessionStart:${e}` ``, BIN off 232675014);
/// every other event uses the bare event name. Verified against the real
/// transcript census: `PostToolUse:Bash` (15 059), `PostToolUse:Edit` (4 623),
/// `Stop` (1 241), `SessionStart:startup` (108), `SessionStart:compact` (55),
/// `SessionStart:clear` (13), `UserPromptSubmit` (1).
#[must_use]
pub fn hook_name_for_event(event: &HookEvent) -> String {
    let base = format!("{:?}", event.event_type());
    match event {
        HookEvent::PreToolUse { tool_name, .. }
        | HookEvent::PostToolUse { tool_name, .. }
        | HookEvent::PostToolUseFailure { tool_name, .. } => format!("{base}:{tool_name}"),
        HookEvent::SessionStart { source, .. } => format!("{base}:{source}"),
        _ => base,
    }
}

/// claude's `toolUseID` for an event, when the event carries one.
///
/// Tool events reuse the provider tool-use id verbatim (`toolu_…` — 24 497 of
/// the mined records). Every other event gets a freshly minted uuid at the
/// runner (`` let l=a||`hook-${randomUUID()}` ``); the caller mints it, so this
/// returns `None` rather than inventing one here.
#[must_use]
pub fn tool_use_id_for_event(event: &HookEvent) -> Option<String> {
    match event {
        HookEvent::PreToolUse { tool_use_id, .. }
        | HookEvent::PostToolUse { tool_use_id, .. }
        | HookEvent::PostToolUseFailure { tool_use_id, .. } => {
            Some(tool_use_id.as_str().to_string())
        }
        _ => None,
    }
}

/// claude's `command` field for a hook — `qq(hook)` (BIN off 230279724):
/// `statusMessage` when set, else `iSe(hook)` (BIN off 230279432):
///
/// ```text
/// function iSe(e){switch(e.type){
///   case"command": return e.args?[e.command,...e.args].join(" "):e.command;
///   case"prompt":  return e.prompt;
///   case"agent":   return e.prompt;
///   case"http":    return e.url;
///   case"mcp_tool":return `${e.server}/${e.tool}`;
///   case"callback":return "callback";
///   case"function":return "function"}}
/// function qq(e){if("statusMessage"in e&&e.statusMessage)return e.statusMessage;
///                return iSe(e)}
/// ```
///
/// The `mcp_tool` arm is now ported ([`HookExecutor::McpTool`]) and renders
/// server, slash, tool — the oracle's own `iSe` case, verified in the 2.1.238
/// binary at offset 296901778. The port still has no `callback` / `function`
/// executor arms; its `Builtin` arm is the nearest analogue of claude's
/// in-process `callback` hook, so it renders as the handler id.
#[must_use]
pub fn attachment_command(hook: &HookDefinition) -> String {
    if let Some(status) = hook.status_message.as_ref() {
        if !status.is_empty() {
            return status.clone();
        }
    }
    match &hook.executor {
        // Never echo the hook BODY into a user-visible status line: it is
        // plugin-authored text of arbitrary length and shape.
        HookExecutor::Function { .. } => format!("function hook {}", hook.name),
        HookExecutor::Command { command, args, .. } => {
            if args.is_empty() {
                command.clone()
            } else {
                let mut parts = Vec::with_capacity(args.len() + 1);
                parts.push(command.as_str());
                parts.extend(args.iter().map(String::as_str));
                parts.join(" ")
            }
        }
        HookExecutor::Http { url, .. } => url.clone(),
        HookExecutor::Agent { prompt, .. } | HookExecutor::Prompt { prompt, .. } => prompt.clone(),
        HookExecutor::McpTool { server, tool, .. } => format!("{server}/{tool}"),
        HookExecutor::Builtin { handler_id } => handler_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O2: `hook_blocking_error` is the ONLY one of the five O2 types whose
    /// field order is compatible with [`identity_head`] — the payload rides in
    /// the fifth slot. The nested `blockingError` object's own key order
    /// (`blockingError`, `command`) is locked by BIN off 237775430.
    #[test]
    fn blocking_error_matches_oracle_key_order() {
        let v = blocking_error_attachment(
            &ident(),
            &BlockingError {
                blocking_error: "[./x.sh]: boom".into(),
                command: "./x.sh".into(),
            },
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_blocking_error","hookName":"PostToolUse:Bash","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse","blockingError":{"blockingError":"[./x.sh]: boom","command":"./x.sh"}}"#
        );
    }

    /// O2: `hook_stopped_continuation` puts `message` SECOND, before the
    /// identity keys — so it must NOT route through [`identity_head`].
    #[test]
    fn stopped_continuation_matches_oracle_key_order() {
        let v = stopped_continuation_attachment(&ident(), "Execution stopped by PostToolUse hook");
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_stopped_continuation","message":"Execution stopped by PostToolUse hook","hookName":"PostToolUse:Bash","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse"}"#
        );
    }

    /// O2: `hook_system_message` puts `content` SECOND, like
    /// `hook_additional_context` — but `content` is a STRING, not an array.
    #[test]
    fn system_message_matches_oracle_key_order() {
        let v = system_message_attachment(&ident(), "reformatted 3 files");
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_system_message","content":"reformatted 3 files","hookName":"PostToolUse:Bash","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse"}"#
        );
    }

    /// `content` passes through `jKe` — the same 10 000-char inline cap every
    /// other hook-output field uses.
    #[test]
    fn system_message_content_is_capped_at_the_inline_limit() {
        let long = "x".repeat(HOOK_OUTPUT_INLINE_LIMIT + 500);
        let v = system_message_attachment(&ident(), &long);
        assert_eq!(
            v["content"].as_str().unwrap().chars().count(),
            HOOK_OUTPUT_INLINE_LIMIT
        );
    }

    /// O2: `hook_deferred_tool` has NO identity block at all — `toolUseID`
    /// leads, `hookName` sits SIXTH, and it carries a `permissionMode` no
    /// other hook attachment has.
    #[test]
    fn deferred_tool_matches_oracle_key_order() {
        let v = deferred_tool_attachment(
            "toolu_01ApkBwAZMCAza47B5nAWiGS",
            "Bash",
            &serde_json::json!({ "command": "ls" }),
            "PreToolUse:Bash",
            "acceptEdits",
            None,
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_deferred_tool","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","toolName":"Bash","toolInput":{"command":"ls"},"hookName":"PreToolUse:Bash","hookEvent":"PreToolUse","permissionMode":"acceptEdits"}"#
        );
    }

    #[test]
    fn deferred_tool_appends_traceparent_last_when_present() {
        let v = deferred_tool_attachment(
            "toolu_01ApkBwAZMCAza47B5nAWiGS",
            "Bash",
            &serde_json::json!({ "command": "ls" }),
            "PreToolUse:Bash",
            "acceptEdits",
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_deferred_tool","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","toolName":"Bash","toolInput":{"command":"ls"},"hookName":"PreToolUse:Bash","hookEvent":"PreToolUse","permissionMode":"acceptEdits","traceparent":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"}"#
        );
    }

    /// The model-facing rendering, byte-locked beside the payload it derives
    /// from. `hook_blocking_error` is one of the few hook attachments the model
    /// actually sees; the `hook_stopped_continuation` prose has no helper here
    /// because its three call sites already inline (and byte-lock) the string.
    #[test]
    fn blocking_error_prose_matches_the_oracle_normalizer() {
        assert_eq!(
            blocking_error_prose(
                "PostToolUse:Bash",
                &BlockingError {
                    blocking_error: "boom".into(),
                    command: "./x.sh".into(),
                },
            ),
            r#"PostToolUse:Bash hook blocking error from command: "./x.sh": boom"#
        );
    }

    fn ident() -> HookAttachmentIdentity {
        HookAttachmentIdentity {
            hook_name: "PostToolUse:Bash".into(),
            hook_event: "PostToolUse".into(),
            tool_use_id: "toolu_01ApkBwAZMCAza47B5nAWiGS".into(),
        }
    }

    /// O3: `hook_additional_context` leads with `content` (an ARRAY of
    /// strings) BEFORE the identity keys — a different order from every
    /// hook-RUN attachment, which starts with `hookName`.
    #[test]
    fn additional_context_matches_oracle_key_order() {
        let v = additional_context_attachment(
            "PostToolUse:Edit",
            "toolu_01ApkBwAZMCAza47B5nAWiGS",
            "PostToolUse",
            &["a".to_string(), "b".to_string()],
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_additional_context","content":["a","b"],"hookName":"PostToolUse:Edit","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse"}"#
        );
    }

    /// The SessionStart producer uses three LITERALS — the hook name is bare
    /// `"SessionStart"` (NOT `SessionStart:{source}`, unlike the hook-run
    /// attachments' `hook_name_for_event`) and `toolUseID` is the literal
    /// string `"SessionStart"`, not a uuid (BIN off 232675554; 129 real
    /// 2.1.220 records).
    #[test]
    fn session_start_additional_context_uses_the_literal_identity() {
        let v = additional_context_attachment(
            "SessionStart",
            "SessionStart",
            "SessionStart",
            &["ctx".to_string()],
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_additional_context","content":["ctx"],"hookName":"SessionStart","toolUseID":"SessionStart","hookEvent":"SessionStart"}"#
        );
    }

    /// O3: `hook_error_during_execution` (consumer arm) — `content` is a
    /// STRING, not an array, and rides in the same second slot.
    #[test]
    fn error_during_execution_consumer_arm_key_order() {
        let v = error_during_execution_attachment(
            "boom",
            "PostToolUse:Edit",
            "toolu_01ApkBwAZMCAza47B5nAWiGS",
            "PostToolUse",
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_error_during_execution","content":"boom","hookName":"PostToolUse:Edit","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse"}"#
        );
    }

    #[test]
    fn success_matches_oracle_key_order() {
        let v = success_attachment(&ident(), "ok", "out\n", "", 0, "./hooks/fmt.sh", 37);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_success","hookName":"PostToolUse:Bash","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse","content":"ok","stdout":"out\n","stderr":"","exitCode":0,"command":"./hooks/fmt.sh","durationMs":37}"#
        );
    }

    #[test]
    fn non_blocking_error_matches_oracle_key_order() {
        let id = HookAttachmentIdentity {
            hook_name: "UserPromptSubmit".into(),
            hook_event: "UserPromptSubmit".into(),
            tool_use_id: "9b7a1c2f-2212-4d41-a588-38d55c6a13cd".into(),
        };
        let v = non_blocking_error_attachment(
            &id,
            "Failed with non-blocking status code: boom",
            "",
            1,
            Some("${LINGXI_PLUGIN_ROOT}/scripts/on-prompt-submit.sh"),
            Some(2),
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_non_blocking_error","hookName":"UserPromptSubmit","toolUseID":"9b7a1c2f-2212-4d41-a588-38d55c6a13cd","hookEvent":"UserPromptSubmit","stderr":"Failed with non-blocking status code: boom","stdout":"","exitCode":1,"command":"${LINGXI_PLUGIN_ROOT}/scripts/on-prompt-submit.sh","durationMs":2}"#
        );
    }

    #[test]
    fn non_blocking_error_omits_command_and_duration_for_transport_arms() {
        // HTTP / mcp_tool arms yield the payload WITHOUT `command`/`durationMs`
        // (BIN off 237799357 / 237800962) — the keys must be ABSENT, not null.
        let v = non_blocking_error_attachment(
            &ident(),
            "HTTP 500 from https://h/",
            "",
            500,
            None,
            None,
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_non_blocking_error","hookName":"PostToolUse:Bash","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse","stderr":"HTTP 500 from https://h/","stdout":"","exitCode":500}"#
        );
    }

    #[test]
    fn cancelled_with_timeout_matches_oracle_key_order() {
        let id = HookAttachmentIdentity {
            hook_name: "Stop".into(),
            hook_event: "Stop".into(),
            tool_use_id: "68c53d21-9374-46a9-b5a0-0ccf3659e9bb".into(),
        };
        let v = cancelled_attachment(
            &id,
            Some("${LINGXI_PLUGIN_ROOT}/scripts/on-stop.sh"),
            Some(591),
            Some(CancellationTimeout {
                timed_out: false,
                timeout_ms: 600_000,
            }),
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_cancelled","hookName":"Stop","toolUseID":"68c53d21-9374-46a9-b5a0-0ccf3659e9bb","hookEvent":"Stop","command":"${LINGXI_PLUGIN_ROOT}/scripts/on-stop.sh","durationMs":591,"timedOut":false,"timeoutMs":600000}"#
        );
    }

    #[test]
    fn cancelled_without_timeout_omits_all_four_optional_keys() {
        // prompt/agent-arm early return (BIN off 237798423) yields ONLY the four
        // identity keys — `timedOut`/`timeoutMs` must be ABSENT, not null.
        let v = cancelled_attachment(&ident(), None, None, None);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"type":"hook_cancelled","hookName":"PostToolUse:Bash","toolUseID":"toolu_01ApkBwAZMCAza47B5nAWiGS","hookEvent":"PostToolUse"}"#
        );
    }

    #[test]
    fn hook_name_is_tool_qualified_for_tool_events_and_source_qualified_for_session_start() {
        use crate::events::HookEvent;
        use protocol::{SessionId, ToolUseId};

        let post = HookEvent::PostToolUse {
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({}),
            tool_output: serde_json::json!({}),
            tool_use_id: ToolUseId::from("toolu_x".to_string()),
            duration_ms: None,
        };
        assert_eq!(hook_name_for_event(&post), "PostToolUse:Bash");
        assert_eq!(
            tool_use_id_for_event(&post).as_deref(),
            Some("toolu_x"),
            "tool events reuse the tool_use id verbatim"
        );

        let start = HookEvent::SessionStart {
            session_id: SessionId::new(),
            source: "startup".into(),
        };
        assert_eq!(hook_name_for_event(&start), "SessionStart:startup");
        assert!(
            tool_use_id_for_event(&start).is_none(),
            "non-tool events mint a fresh uuid instead"
        );

        let stop = HookEvent::Stop {
            reason: "end_turn".into(),
        };
        assert_eq!(hook_name_for_event(&stop), "Stop");

        let model_switch = HookEvent::PostModelSwitch {
            from_model: "claude-sonnet-4-6".into(),
            to_model: "claude-opus-4-6".into(),
            requested_model: Some("opus".into()),
            source: "picker".into(),
            context_tokens: 0,
            prompt_cache_warm: false,
            cache_ttl: "5m".into(),
            estimated_cache_write_usd: 0.0,
            pricing: "default".into(),
        };
        assert_eq!(hook_name_for_event(&model_switch), "PostModelSwitch");
        assert!(tool_use_id_for_event(&model_switch).is_none());
    }

    #[test]
    fn command_string_prefers_status_message_then_renders_per_executor_arm() {
        use crate::definition::{HookDefinition, HookExecutor, HookSource};
        use crate::events::HookEventType;
        use std::collections::HashMap;

        let base = |executor: HookExecutor| HookDefinition {
            id: protocol::HookId::new(),
            name: "h".into(),
            events: vec![HookEventType::Stop],
            if_condition: None,
            executor,
            source: HookSource::Settings(protocol::SettingsScope::User),
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        };

        let cmd = base(HookExecutor::Command {
            command: "./x.sh".into(),
            args: vec!["-a".into(), "b".into()],
            env: HashMap::new(),
            cwd: None,
            shell: None,
        });
        assert_eq!(attachment_command(&cmd), "./x.sh -a b");

        let http = base(HookExecutor::Http {
            url: "https://h/hook".into(),
            method: "POST".into(),
            headers: HashMap::new(),
            allowed_env_vars: vec![],
            timeout: std::time::Duration::from_secs(1),
        });
        assert_eq!(attachment_command(&http), "https://h/hook");

        // SH-04: `y2e` (2.1.238 @ 296901596) renders an `mcp_tool` hook as
        // server, slash, tool.
        let mcp = base(HookExecutor::McpTool {
            server: "linter".into(),
            tool: "format_file".into(),
            input: HashMap::new(),
        });
        assert_eq!(attachment_command(&mcp), "linter/format_file");

        let mut with_status = base(HookExecutor::Command {
            command: "./x.sh".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            shell: None,
        });
        with_status.status_message = Some("Formatting".into());
        assert_eq!(
            attachment_command(&with_status),
            "Formatting",
            "`qq` returns statusMessage when set, else `iSe`"
        );
    }
}
